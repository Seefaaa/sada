//! Allocation of outgoing media slots to remote speakers.
//!
//! Each peer receives one negotiated send-only m-line per speaker it can hear. Slots are scarce: adding one costs a
//! full SDP renegotiation round trip, so they are pooled rather than grown without bound.
//!
//! A slot is freed when its speaker disconnects, and otherwise taken from whoever has been quiet longest once
//! somebody new needs one. Reassignment is cheap where renegotiation is not, so the pool settles at the number of
//! people talking at once rather than at the number who have ever talked.
//!
//! The table is generic over the slot type so it can be exercised without constructing WebRTC state; the SFU
//! instantiates it with [`str0m::media::Mid`]. Time is passed in rather than read here, for the same reason.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use sada_common::SessionId;

/// Upper bound on negotiated slots per peer.
///
/// Every slot is an m-line carried in every future offer and answer, roughly 975 bytes of SDP each, and
/// `setRemoteDescription` cost grows with the m-line count, so the pool is not allowed to grow without bound.
///
/// This bounds *simultaneous* speakers: a slot whose speaker has been quiet for [`IDLE_THRESHOLD`] is taken by the
/// next person who needs one, so reaching the ceiling means 24 people talking inside the same few seconds.
///
/// At the ceiling, and only while every slot is genuinely busy, a further speaker is refused and goes unheard until
/// one falls idle. Taking a busy slot instead would be worse: with more speakers than slots, every frame would steal
/// the slot the previous frame just took, and each of the 24 streams would carry a different voice every 20 ms. One
/// person silent is better than everybody chopped up, and the choice is stable; whoever is without a slot stays
/// without it, rather than the whole room taking turns being broken.
pub const MAX_SLOTS: usize = 24;

/// How long a slot must go unused before another speaker may take it.
///
/// Longer than any pause inside a talkspurt, and longer than a quick un-key and re-key, so an active speaker never
/// loses their slot mid-sentence. Short enough that the pool settles at the set of people who spoke in the last few
/// seconds instead of ratcheting up to [`MAX_SLOTS`] and staying there, which matters because every slot is an
/// m-line carried in every future offer and answer.
const IDLE_THRESHOLD: Duration = Duration::from_secs(5);

/// A slot dedicated to a speaker, and when it last carried one of their frames.
#[derive(Debug)]
struct Assignment<S> {
    /// The slot itself.
    slot: S,
    /// When [`SlotTable::slot_for`] last handed this out, which is when the speaker was last heard.
    last_used: Instant,
}

/// The outcome of asking for a slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Grant<S> {
    /// A slot the speaker already held, or one that was free.
    Ready(S),
    /// A slot taken from a speaker who had gone quiet.
    ///
    /// The output clock on it is still the previous speaker's, so the caller has to reset it before writing; see
    /// [`SlotTimeline::release`](crate::sfu::timeline::SlotTimeline::release).
    Reclaimed(S),
    /// Nothing was free and nothing had been idle long enough to take.
    ///
    /// The caller either grows the pool or, at [`MAX_SLOTS`], leaves this speaker unheard.
    Denied,
}

/// Pool of outgoing media slots, and their current assignment to speakers.
#[derive(Debug)]
pub struct SlotTable<S> {
    /// Negotiated slots not currently carrying a speaker.
    available: Vec<S>,
    /// Slots currently dedicated to a speaker, and when each was last used.
    assigned: HashMap<SessionId, Assignment<S>>,
}

impl<S: Copy> SlotTable<S> {
    /// Create an empty table.
    ///
    /// A peer starts with no slots at all; the first speaker it needs to hear triggers the first renegotiation.
    #[must_use]
    pub fn new() -> Self {
        Self {
            available: Vec::new(),
            assigned: HashMap::new(),
        }
    }

    /// Add freshly negotiated slots to the pool.
    pub fn add_negotiated(&mut self, slots: impl IntoIterator<Item = S>) { self.available.extend(slots); }

    /// Return the slot carrying `speaker`, finding one if they have none.
    ///
    /// A free slot is preferred; failing that, the slot of whoever has been quiet longest is taken, but only once
    /// they have been quiet for [`IDLE_THRESHOLD`]. [`Grant::Denied`] is the caller's signal to renegotiate for more.
    ///
    /// `now` is passed in rather than read so that the table stays a pure data structure with synthetic tests. It is
    /// also what marks the speaker as heard, which is what keeps them from being the next one reclaimed.
    pub fn slot_for(&mut self, speaker: SessionId, now: Instant) -> Grant<S> {
        if let Some(assignment) = self.assigned.get_mut(&speaker) {
            assignment.last_used = now;
            return Grant::Ready(assignment.slot);
        }

        if let Some(slot) = self.available.pop() {
            self.assigned.insert(speaker, Assignment { slot, last_used: now });
            return Grant::Ready(slot);
        }

        let Some(quietest) = self.quietest(now) else {
            return Grant::Denied;
        };
        let Some(assignment) = self.assigned.remove(&quietest) else {
            return Grant::Denied;
        };

        self.assigned.insert(
            speaker,
            Assignment {
                slot: assignment.slot,
                last_used: now,
            },
        );

        Grant::Reclaimed(assignment.slot)
    }

    /// The speaker whose slot is worth taking, if anyone's is.
    ///
    /// A linear scan, because there are at most [`MAX_SLOTS`] assignments and keeping them in recency order would
    /// cost more on every frame than this costs on the rare frame that finds the pool dry.
    fn quietest(&self, now: Instant) -> Option<SessionId> {
        let (speaker, assignment) = self.assigned.iter().min_by_key(|(_, a)| a.last_used)?;
        (now.duration_since(assignment.last_used) >= IDLE_THRESHOLD).then_some(*speaker)
    }

    /// Hand a speaker's slot back to the pool.
    ///
    /// Called when a speaker disconnects. A speaker who merely goes quiet keeps their slot until somebody else needs
    /// it, which [`slot_for`](Self::slot_for) handles without going through the pool.
    pub fn release(&mut self, speaker: SessionId) -> Option<S> {
        let assignment = self.assigned.remove(&speaker)?;
        self.available.push(assignment.slot);
        Some(assignment.slot)
    }

    /// Total number of slots this peer has negotiated.
    #[must_use]
    pub fn capacity(&self) -> usize { self.assigned.len() + self.available.len() }

    /// Whether every negotiated slot is in use.
    #[must_use]
    pub fn is_exhausted(&self) -> bool { self.available.is_empty() }

    /// How many slots to add when the pool runs dry.
    ///
    /// Doubling keeps the number of renegotiations logarithmic in the number of simultaneous speakers, which matters
    /// because each one is a full SDP round trip to the browser.
    ///
    /// Returns zero once [`MAX_SLOTS`] is reached, which is the caller's signal to stop renegotiating and leave the
    /// speakers it cannot fit unheard. Growth is only reached for at all when no slot has fallen idle, so the pool
    /// tracks concurrent speakers rather than every speaker ever heard; see [`MAX_SLOTS`].
    #[must_use]
    pub fn growth_target(&self) -> usize {
        let current = self.capacity();
        current.max(1).min(MAX_SLOTS.saturating_sub(current))
    }
}

impl<S: Copy> Default for SlotTable<S> {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use sada_common::SessionId;

    use super::{Grant, IDLE_THRESHOLD, MAX_SLOTS, SlotTable};

    /// Build a session id for tests.
    fn session(raw: u64) -> SessionId { SessionId::from_raw(raw) }

    /// A moment to measure the synthetic clock from.
    fn start() -> Instant { Instant::now() }

    /// `seconds` after `from`.
    fn later(from: Instant, seconds: u64) -> Instant { from + Duration::from_secs(seconds) }

    /// The slot a grant carries, or `None` if it was refused.
    fn slot<S>(grant: Grant<S>) -> Option<S> {
        match grant {
            Grant::Ready(slot) | Grant::Reclaimed(slot) => Some(slot),
            Grant::Denied => None,
        }
    }

    #[test]
    fn empty_table_hands_out_nothing() {
        let mut table = SlotTable::<u8>::new();
        assert_eq!(table.slot_for(session(1), start()), Grant::Denied);
        assert_eq!(table.capacity(), 0);
        assert!(table.is_exhausted());
    }

    #[test]
    fn a_speaker_keeps_its_slot() {
        let now = start();
        let mut table = SlotTable::new();

        table.add_negotiated([10, 20]);

        let first = table.slot_for(session(1), now);
        let again = table.slot_for(session(1), later(now, 1));

        assert_eq!(first, again);
    }

    #[test]
    fn distinct_speakers_get_distinct_slots() {
        let now = start();
        let mut table = SlotTable::new();

        table.add_negotiated([10, 20]);

        let first = slot(table.slot_for(session(1), now)).unwrap();
        let second = slot(table.slot_for(session(2), now)).unwrap();

        assert_ne!(first, second);
        assert!(table.is_exhausted());

        // Both slots are busy, so the third speaker has to wait for the pool to grow.
        assert_eq!(table.slot_for(session(3), now), Grant::Denied);
    }

    #[test]
    fn released_slots_are_reused() {
        let now = start();
        let mut table = SlotTable::new();

        table.add_negotiated([10]);

        let first = slot(table.slot_for(session(1), now)).unwrap();

        assert_eq!(table.slot_for(session(2), now), Grant::Denied);
        assert_eq!(table.release(session(1)), Some(first));

        // Capacity is unchanged: the slot was recycled, not renegotiated away.
        assert_eq!(table.slot_for(session(2), now), Grant::Ready(first));
        assert_eq!(table.capacity(), 1);
    }

    #[test]
    fn releasing_an_unknown_speaker_is_harmless() {
        let mut table = SlotTable::<u8>::new();
        assert_eq!(table.release(session(9)), None);
        assert_eq!(table.capacity(), 0);
    }

    #[test]
    fn an_idle_speakers_slot_is_reclaimed() {
        let now = start();
        let mut table = SlotTable::new();

        table.add_negotiated([10]);

        let first = slot(table.slot_for(session(1), now)).unwrap();
        let idle = later(now, IDLE_THRESHOLD.as_secs());

        // The same slot changes hands, so the caller has to reset its clock, and no renegotiation is needed.
        assert_eq!(table.slot_for(session(2), idle), Grant::Reclaimed(first));
        assert_eq!(table.capacity(), 1);
    }

    #[test]
    fn a_busy_speakers_slot_is_left_alone() {
        let now = start();
        let mut table = SlotTable::new();

        table.add_negotiated([10]);
        table.slot_for(session(1), now);

        // Still mid-sentence: growing the pool is the right answer, not interrupting them.
        assert_eq!(table.slot_for(session(2), later(now, 1)), Grant::Denied);
    }

    #[test]
    fn the_quietest_speaker_loses_their_slot() {
        let now = start();
        let mut table = SlotTable::new();

        table.add_negotiated([10, 20]);

        let first = slot(table.slot_for(session(1), now)).unwrap();
        slot(table.slot_for(session(2), later(now, 1))).unwrap();

        assert_eq!(table.slot_for(session(3), later(now, 10)), Grant::Reclaimed(first));
    }

    #[test]
    fn using_a_slot_keeps_it_from_being_reclaimed() {
        let now = start();
        let mut table = SlotTable::new();

        table.add_negotiated([10, 20]);

        slot(table.slot_for(session(1), now)).unwrap();
        let second = slot(table.slot_for(session(2), later(now, 1))).unwrap();

        // The speaker who started first is still talking, so the one who fell quiet is the one who pays.
        table.slot_for(session(1), later(now, 9));

        assert_eq!(table.slot_for(session(3), later(now, 10)), Grant::Reclaimed(second));
    }

    #[test]
    fn a_speaker_at_the_ceiling_is_refused_while_everyone_is_talking() {
        let now = start();
        let mut table = SlotTable::new();

        table.add_negotiated(0..u8::try_from(MAX_SLOTS).unwrap());

        for speaker in 0..MAX_SLOTS as u64 {
            slot(table.slot_for(session(speaker), now)).unwrap();
        }

        // Nothing left to grow into and nobody quiet: the newcomer goes unheard rather than chopping up a stream
        // that is in use.
        assert_eq!(table.growth_target(), 0);
        assert_eq!(table.slot_for(session(99), later(now, 1)), Grant::Denied);
    }

    #[test]
    fn growth_stops_at_the_slot_ceiling() {
        let mut table = SlotTable::new();

        table.add_negotiated(0..u8::try_from(MAX_SLOTS).unwrap());

        assert_eq!(table.capacity(), MAX_SLOTS);
        assert_eq!(table.growth_target(), 0);
    }

    #[test]
    fn growth_never_overshoots_the_ceiling() {
        let mut table = SlotTable::new();

        table.add_negotiated(0..u8::try_from(MAX_SLOTS - 2).unwrap());

        assert_eq!(table.capacity() + table.growth_target(), MAX_SLOTS);
    }

    #[test]
    fn growth_doubles_capacity() {
        let mut table = SlotTable::<u8>::new();

        // From nothing, ask for a single slot rather than none.
        assert_eq!(table.growth_target(), 1);

        table.add_negotiated([10]);

        assert_eq!(table.growth_target(), 1);

        table.add_negotiated([20, 30]);

        assert_eq!(table.growth_target(), 3);
    }
}
