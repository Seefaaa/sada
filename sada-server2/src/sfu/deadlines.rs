//! Tracking of when each peer next needs to be driven forward.
//!
//! str0m is sans-I/O: it never reads the clock itself and instead reports, via
//! `Output::Timeout`, the instant at which it next wants attention. Rather than
//! polling every peer on every loop iteration, the worker keeps those instants
//! here and only wakes the peers whose deadline has actually passed.

use std::{
    collections::{BTreeSet, HashMap},
    time::{Duration, Instant},
};

use sada_common::SessionId;

/// Deadlines further out than this are treated as "no deadline at all".
///
/// str0m reports `Instant + 100 years` when a peer has nothing scheduled. Such
/// a value is useless to sleep on and would keep an entry alive forever, so it
/// is dropped instead.
const HORIZON: Duration = Duration::from_secs(3600);

/// Set of pending per-peer deadlines, ordered by time.
#[derive(Debug, Default)]
pub struct Deadlines {
    /// Deadlines in chronological order. The session id breaks ties so that two
    /// peers scheduled at the same instant both keep an entry.
    ordered: BTreeSet<(Instant, SessionId)>,
    /// Reverse index used to find and replace a peer's existing deadline.
    by_session: HashMap<SessionId, Instant>,
}

impl Deadlines {
    /// Create an empty set.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Record when `session` next needs attention, replacing any previous entry.
    ///
    /// Deadlines beyond [`HORIZON`] are discarded rather than stored, so a peer
    /// with nothing scheduled simply has no entry.
    pub fn set(&mut self, session: SessionId, at: Instant, now: Instant) {
        self.remove(session);

        if at.saturating_duration_since(now) >= HORIZON {
            return;
        }

        self.ordered.insert((at, session));
        self.by_session.insert(session, at);
    }

    /// Drop any deadline for `session`.
    pub fn remove(&mut self, session: SessionId) {
        if let Some(previous) = self.by_session.remove(&session) {
            self.ordered.remove(&(previous, session));
        }
    }

    /// The earliest pending deadline, if any.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> { self.ordered.first().map(|&(at, _)| at) }

    /// Remove and return every session whose deadline is at or before `now`.
    pub fn take_expired(&mut self, now: Instant) -> Vec<SessionId> {
        let mut expired = Vec::new();

        while let Some(&(at, session)) = self.ordered.first() {
            if at > now {
                break;
            }
            self.ordered.pop_first();
            self.by_session.remove(&session);
            expired.push(session);
        }

        expired
    }

    /// Number of tracked deadlines.
    #[cfg(test)]
    fn len(&self) -> usize { self.ordered.len() }

    /// Whether no deadline is pending.
    #[cfg(test)]
    fn is_empty(&self) -> bool { self.ordered.is_empty() }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use sada_common::SessionId;

    use super::{Deadlines, HORIZON};

    /// Build a session id for tests.
    fn session(raw: u64) -> SessionId { SessionId::from_raw(raw) }

    #[test]
    fn earliest_deadline_wins() {
        let now = Instant::now();
        let mut deadlines = Deadlines::new();

        deadlines.set(session(1), now + Duration::from_millis(50), now);
        deadlines.set(session(2), now + Duration::from_millis(10), now);

        assert_eq!(deadlines.next_deadline(), Some(now + Duration::from_millis(10)));
    }

    #[test]
    fn setting_again_replaces_the_previous_entry() {
        let now = Instant::now();
        let mut deadlines = Deadlines::new();

        deadlines.set(session(1), now + Duration::from_millis(50), now);
        deadlines.set(session(1), now + Duration::from_millis(10), now);

        assert_eq!(deadlines.len(), 1);
        assert_eq!(deadlines.next_deadline(), Some(now + Duration::from_millis(10)));
    }

    #[test]
    fn far_future_deadlines_are_not_stored() {
        let now = Instant::now();
        let mut deadlines = Deadlines::new();

        // This is what str0m reports for a peer with nothing scheduled.
        deadlines.set(session(1), now + HORIZON * 2, now);

        assert!(deadlines.is_empty());
        assert_eq!(deadlines.next_deadline(), None);
    }

    #[test]
    fn a_far_future_deadline_clears_a_pending_one() {
        let now = Instant::now();
        let mut deadlines = Deadlines::new();

        deadlines.set(session(1), now + Duration::from_millis(10), now);
        deadlines.set(session(1), now + HORIZON * 2, now);

        assert!(deadlines.is_empty());
    }

    #[test]
    fn expired_deadlines_are_taken_once() {
        let now = Instant::now();
        let mut deadlines = Deadlines::new();

        deadlines.set(session(1), now, now);
        deadlines.set(session(2), now + Duration::from_millis(5), now);
        deadlines.set(session(3), now + Duration::from_secs(1), now);

        let expired = deadlines.take_expired(now + Duration::from_millis(10));

        assert_eq!(expired, vec![session(1), session(2)]);
        assert_eq!(deadlines.len(), 1);
        assert!(deadlines.take_expired(now + Duration::from_millis(10)).is_empty());
    }

    #[test]
    fn removing_a_session_drops_its_deadline() {
        let now = Instant::now();
        let mut deadlines = Deadlines::new();

        deadlines.set(session(1), now + Duration::from_millis(10), now);
        deadlines.remove(session(1));

        assert!(deadlines.is_empty());
        // Removing twice is harmless.
        deadlines.remove(session(1));
    }

    #[test]
    fn deadlines_at_the_same_instant_both_survive() {
        let now = Instant::now();
        let at = now + Duration::from_millis(10);
        let mut deadlines = Deadlines::new();

        deadlines.set(session(1), at, now);
        deadlines.set(session(2), at, now);

        assert_eq!(deadlines.len(), 2);
        assert_eq!(deadlines.take_expired(at).len(), 2);
    }
}
