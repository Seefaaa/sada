//! Storage for the worker's peers.
//!
//! Peers live in a slab rather than a map for two reasons.
//!
//! Relaying audio needs a mutable borrow of the speaker *and* of every listener at the same time, which a `HashMap`
//! cannot give. A slab can: the speaker is lifted out with [`Peers::take`] for the duration of the fan-out and put back
//! afterwards.
//!
//! Slots are also recycled, and a stale identifier that silently starts addressing whoever took the slot over would
//! route audio to the wrong player. Each slot therefore carries a generation, and the identifier carries it too, so a
//! stale id fails to resolve instead of aliasing.

use sada_common::SessionId;

/// One storage slot.
#[derive(Debug)]
struct Slot<T> {
    /// Incremented every time the slot is released, invalidating old ids.
    generation: u32,
    /// The peer, absent while it is lifted out for a fan-out.
    value: Option<T>,
    /// Whether a session owns this slot, even if its value is lifted out.
    occupied: bool,
}

/// Generation-tagged slab of peers.
#[derive(Debug)]
pub struct Peers<T> {
    /// Backing storage, indexed by slot.
    slots: Vec<Slot<T>>,
    /// Slots available for reuse.
    free: Vec<u32>,
    /// Number of occupied slots.
    len: usize,
}

impl<T> Peers<T> {
    /// Create an empty slab.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            len: 0,
        }
    }

    /// Store a peer and return the id that addresses it.
    pub fn insert(&mut self, value: T) -> SessionId {
        self.len += 1;

        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.value = Some(value);
            slot.occupied = true;
            return SessionId::new(index, slot.generation);
        }

        let index = u32::try_from(self.slots.len()).expect("a server never holds 2^32 concurrent sessions");

        self.slots.push(Slot {
            generation: 1,
            value: Some(value),
            occupied: true,
        });

        SessionId::new(index, 1)
    }

    /// Borrow the peer `id` addresses.
    ///
    /// Returns `None` for a stale id, and also while the peer is lifted out.
    #[must_use]
    pub fn get(&self, id: SessionId) -> Option<&T> { self.slot(id)?.value.as_ref() }

    /// Mutably borrow the peer `id` addresses.
    pub fn get_mut(&mut self, id: SessionId) -> Option<&mut T> { self.slot_mut(id)?.value.as_mut() }

    /// Lift a peer out, keeping its slot reserved.
    ///
    /// The caller must put it back with [`Peers::restore`]; until then the id
    /// does not resolve and the slot cannot be reused.
    #[must_use]
    pub fn take(&mut self, id: SessionId) -> Option<T> { self.slot_mut(id)?.value.take() }

    /// Put back a peer lifted out by [`Peers::take`].
    ///
    /// Returns the value unchanged if the slot went away in the meantime, which can only happen if the peer was removed
    /// while lifted out.
    pub fn restore(&mut self, id: SessionId, value: T) -> Option<T> {
        match self.slot_mut(id) {
            Some(slot) => {
                slot.value = Some(value);
                None
            },
            None => Some(value),
        }
    }

    /// Remove a peer, freeing its slot for reuse.
    pub fn remove(&mut self, id: SessionId) -> Option<T> {
        let slot = self.slot_mut(id)?;

        let value = slot.value.take();

        slot.occupied = false;
        slot.generation = slot.generation.wrapping_add(1).max(1);

        self.free.push(id.slot());
        self.len -= 1;

        value
    }

    /// Iterate over every present peer and its id.
    pub fn iter(&self) -> impl Iterator<Item = (SessionId, &T)> {
        self.slots.iter().enumerate().filter_map(|(index, slot)| {
            let value = slot.value.as_ref()?;
            let index = u32::try_from(index).ok()?;
            Some((SessionId::new(index, slot.generation), value))
        })
    }

    /// Ids of every occupied slot, including any currently lifted out.
    pub fn ids(&self) -> Vec<SessionId> {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.occupied)
            .filter_map(|(index, slot)| Some(SessionId::new(u32::try_from(index).ok()?, slot.generation)))
            .collect()
    }

    /// Number of occupied slots.
    #[must_use]
    pub fn len(&self) -> usize { self.len }

    /// Whether no peer is stored.
    #[cfg(test)]
    fn is_empty(&self) -> bool { self.len == 0 }

    /// Resolve an id to its slot, rejecting stale generations.
    fn slot(&self, id: SessionId) -> Option<&Slot<T>> {
        let slot = self.slots.get(id.slot() as usize)?;
        (slot.occupied && slot.generation == id.generation()).then_some(slot)
    }

    /// Resolve an id to its slot mutably, rejecting stale generations.
    fn slot_mut(&mut self, id: SessionId) -> Option<&mut Slot<T>> {
        let slot = self.slots.get_mut(id.slot() as usize)?;
        (slot.occupied && slot.generation == id.generation()).then_some(slot)
    }
}

impl<T> Default for Peers<T> {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::Peers;

    #[test]
    fn a_stored_peer_is_addressable() {
        let mut peers = Peers::new();
        let id = peers.insert("a");

        assert_eq!(peers.get(id), Some(&"a"));
        assert_eq!(peers.len(), 1);
    }

    #[test]
    fn a_removed_peer_is_gone() {
        let mut peers = Peers::new();
        let id = peers.insert("a");

        assert_eq!(peers.remove(id), Some("a"));
        assert_eq!(peers.get(id), None);
        assert!(peers.is_empty());
        assert_eq!(peers.remove(id), None);
    }

    #[test]
    fn a_recycled_slot_rejects_the_old_id() {
        let mut peers = Peers::new();
        let old = peers.insert("a");
        peers.remove(old);
        let new = peers.insert("b");

        // The same storage slot, but the old id must not reach the new peer.
        assert_eq!(old.slot(), new.slot());
        assert_ne!(old, new);
        assert_eq!(peers.get(old), None);
        assert_eq!(peers.get(new), Some(&"b"));
    }

    #[test]
    fn removing_with_a_stale_id_leaves_the_new_peer_alone() {
        let mut peers = Peers::new();
        let old = peers.insert("a");
        peers.remove(old);
        let new = peers.insert("b");

        assert_eq!(peers.remove(old), None);
        assert_eq!(peers.get(new), Some(&"b"));
        assert_eq!(peers.len(), 1);
    }

    #[test]
    fn a_lifted_peer_can_be_put_back() {
        let mut peers = Peers::new();
        let id = peers.insert("a");

        let taken = peers.take(id).unwrap();
        assert_eq!(peers.get(id), None);
        // The slot stays reserved, so it is not handed to a new peer.
        assert_eq!(peers.len(), 1);

        assert_eq!(peers.restore(id, taken), None);
        assert_eq!(peers.get(id), Some(&"a"));
    }

    #[test]
    fn a_lifted_slot_is_not_reused() {
        let mut peers = Peers::new();
        let first = peers.insert("a");
        let taken = peers.take(first).unwrap();

        let second = peers.insert("b");
        assert_ne!(first.slot(), second.slot());

        peers.restore(first, taken);
        assert_eq!(peers.get(first), Some(&"a"));
        assert_eq!(peers.get(second), Some(&"b"));
    }

    #[test]
    fn restoring_into_a_vanished_slot_hands_the_value_back() {
        let mut peers = Peers::new();
        let id = peers.insert("a");
        let taken = peers.take(id).unwrap();
        peers.remove(id);

        assert_eq!(peers.restore(id, taken), Some("a"));
    }

    #[test]
    fn iteration_skips_lifted_and_removed_peers() {
        let mut peers = Peers::new();
        let a = peers.insert("a");
        let b = peers.insert("b");
        let c = peers.insert("c");

        peers.remove(b);
        let lifted = peers.take(c).unwrap();

        let present = peers.iter().map(|(_, v)| *v).collect::<Vec<_>>();
        assert_eq!(present, ["a"]);

        // But the lifted peer still owns its slot.
        assert_eq!(peers.ids().len(), 2);

        peers.restore(c, lifted);
        let mut present = peers.iter().map(|(_, v)| *v).collect::<Vec<_>>();
        present.sort_unstable();
        assert_eq!(present, ["a", "c"]);
        assert_eq!(peers.get(a), Some(&"a"));
    }
}
