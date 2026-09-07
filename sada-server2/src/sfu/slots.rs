//! Allocation of outgoing media slots to remote speakers.
//!
//! Each peer receives one negotiated send-only m-line per speaker it can hear. Slots are scarce: adding one costs a
//! full SDP renegotiation round trip, so they are pooled rather than grown without bound.
//!
//! A slot returns to the pool only when its speaker disconnects, which is less often than it should be; see
//! [`MAX_SLOTS`].
//!
//! The table is generic over the slot type so it can be exercised without constructing WebRTC state; the SFU
//! instantiates it with [`str0m::media::Mid`].

use std::collections::HashMap;

use sada_common::SessionId;

/// Upper bound on negotiated slots per peer.
///
/// Every slot is an m-line carried in every future offer and answer, roughly 975 bytes of SDP each, and
/// `setRemoteDescription` cost grows with the m-line count, so the pool is not allowed to grow without bound.
///
/// This is meant to bound *simultaneous* speakers, and does not. [`SlotTable::release`] is reached only when a speaker
/// disconnects, so leaving proximity or a hearer list frees nothing and the real ceiling is the number of *distinct*
/// speakers a listener has heard all round. Once it is reached, [`SlotTable::growth_target`] returns zero and every
/// further speaker is silently unheard for the rest of the session.
pub const MAX_SLOTS: usize = 24;

/// Pool of outgoing media slots, and their current assignment to speakers.
#[derive(Debug)]
pub struct SlotTable<S> {
    /// Negotiated slots not currently carrying a speaker.
    available: Vec<S>,
    /// Slots currently dedicated to a speaker.
    assigned: HashMap<SessionId, S>,
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

    /// Return the slot carrying `speaker`, assigning a free one if needed.
    ///
    /// Returns `None` when the pool is exhausted, which is the caller's signal to renegotiate.
    pub fn slot_for(&mut self, speaker: SessionId) -> Option<S> {
        if let Some(slot) = self.assigned.get(&speaker) {
            return Some(*slot);
        }

        let slot = self.available.pop()?;
        self.assigned.insert(speaker, slot);

        Some(slot)
    }

    /// Hand a speaker's slot back to the pool.
    ///
    /// Only ever called for a speaker that has disconnected. Nothing reclaims a slot from a speaker who is merely out
    /// of earshot, which is the defect described on [`MAX_SLOTS`].
    pub fn release(&mut self, speaker: SessionId) -> Option<S> {
        let slot = self.assigned.remove(&speaker)?;
        self.available.push(slot);
        Some(slot)
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
    /// Returns zero once [`MAX_SLOTS`] is reached, which is the caller's signal to stop renegotiating and start
    /// dropping speakers instead. Because slots are only released on disconnect, that point arrives after this many
    /// distinct speakers rather than this many concurrent ones; see [`MAX_SLOTS`].
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
    use sada_common::SessionId;

    use super::{MAX_SLOTS, SlotTable};

    /// Build a session id for tests.
    fn session(raw: u64) -> SessionId { SessionId::from_raw(raw) }

    #[test]
    fn empty_table_hands_out_nothing() {
        let mut table = SlotTable::<u8>::new();
        assert_eq!(table.slot_for(session(1)), None);
        assert_eq!(table.capacity(), 0);
        assert!(table.is_exhausted());
    }

    #[test]
    fn a_speaker_keeps_its_slot() {
        let mut table = SlotTable::new();

        table.add_negotiated([10, 20]);

        let first = table.slot_for(session(1)).unwrap();
        let again = table.slot_for(session(1)).unwrap();

        assert_eq!(first, again);
    }

    #[test]
    fn distinct_speakers_get_distinct_slots() {
        let mut table = SlotTable::new();

        table.add_negotiated([10, 20]);

        let first = table.slot_for(session(1)).unwrap();
        let second = table.slot_for(session(2)).unwrap();

        assert_ne!(first, second);
        assert!(table.is_exhausted());
        assert_eq!(table.slot_for(session(3)), None);
    }

    #[test]
    fn released_slots_are_reused() {
        let mut table = SlotTable::new();

        table.add_negotiated([10]);

        let first = table.slot_for(session(1)).unwrap();

        assert_eq!(table.slot_for(session(2)), None);
        assert_eq!(table.release(session(1)), Some(first));

        // Capacity is unchanged: the slot was recycled, not renegotiated away.
        assert_eq!(table.slot_for(session(2)), Some(first));
        assert_eq!(table.capacity(), 1);
    }

    #[test]
    fn releasing_an_unknown_speaker_is_harmless() {
        let mut table = SlotTable::<u8>::new();
        assert_eq!(table.release(session(9)), None);
        assert_eq!(table.capacity(), 0);
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
        let mut table = SlotTable::new();

        // From nothing, ask for a single slot rather than none.
        assert_eq!(table.growth_target(), 1);

        table.add_negotiated([10]);

        assert_eq!(table.growth_target(), 1);

        table.add_negotiated([20, 30]);

        assert_eq!(table.growth_target(), 3);
    }
}
