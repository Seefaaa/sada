//! Routing of inbound datagrams to the peer they belong to.
//!
//! Every peer shares one UDP socket, so each datagram has to be attributed to a peer before it can be fed to str0m.
//! str0m offers [`Rtc::accepts`](str0m::Rtc::accepts) for exactly this, but calling it on every peer for every datagram
//! is linear in the number of connected peers.
//!
//! Instead the source address of accepted traffic is remembered here, turning the common case into one hash lookup. The
//! map is a *hint*, not an authority: ICE may renominate a candidate pair mid-call without an ICE restart (see
//! [`str0m/docs/ice.md`](https://github.com/algesten/str0m/blob/main/docs/ice.md)), so a peer's source address can change at any time and a NAT rebinding can even hand an address
//! to a different peer. The worker therefore still confirms the hint with [`Rtc::accepts`](str0m::Rtc::accepts) and
//! calls [`AddressMap::forget_address`] when it turns out to be stale.

use std::{
    collections::{HashMap, hash_map::Entry},
    net::SocketAddr,
};

use sada_common::SessionId;

/// Learned association between remote socket addresses and peers.
#[derive(Debug, Default)]
pub struct AddressMap {
    /// Which peer traffic from an address most recently belonged to.
    by_address: HashMap<SocketAddr, SessionId>,
    /// Addresses attributed to each peer, so they can all be dropped at once.
    by_session: HashMap<SessionId, Vec<SocketAddr>>,
}

impl AddressMap {
    /// Create an empty map.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// The peer that traffic from `address` is believed to belong to.
    ///
    /// The caller must confirm the answer before acting on it.
    #[must_use]
    pub fn lookup(&self, address: SocketAddr) -> Option<SessionId> { self.by_address.get(&address).copied() }

    /// Record that traffic from `address` belongs to `session`.
    ///
    /// If the address was previously attributed to a different peer, the old
    /// attribution is replaced.
    pub fn learn(&mut self, address: SocketAddr, session: SessionId) {
        match self.by_address.entry(address) {
            Entry::Occupied(mut entry) => {
                let previous = entry.insert(session);
                if previous == session {
                    return;
                }
                Self::detach(&mut self.by_session, address, previous);
            },
            Entry::Vacant(entry) => {
                entry.insert(session);
            },
        }

        self.by_session.entry(session).or_default().push(address);
    }

    /// Drop the attribution of a single address.
    pub fn forget_address(&mut self, address: SocketAddr) {
        if let Some(session) = self.by_address.remove(&address) {
            Self::detach(&mut self.by_session, address, session);
        }
    }

    /// Drop every address attributed to `session`.
    pub fn forget_session(&mut self, session: SessionId) {
        if let Some(addresses) = self.by_session.remove(&session) {
            for address in addresses {
                self.by_address.remove(&address);
            }
        }
    }

    /// Number of remembered addresses.
    #[cfg(test)]
    fn len(&self) -> usize { self.by_address.len() }

    /// Whether nothing has been learned yet.
    #[cfg(test)]
    fn is_empty(&self) -> bool { self.by_address.is_empty() }

    /// Remove `address` from `session`'s address list, dropping the list if empty.
    fn detach(by_session: &mut HashMap<SessionId, Vec<SocketAddr>>, address: SocketAddr, session: SessionId) {
        if let Entry::Occupied(mut entry) = by_session.entry(session) {
            entry.get_mut().retain(|&known| known != address);
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use sada_common::SessionId;

    use super::AddressMap;

    /// Build a session id for tests.
    fn session(raw: u64) -> SessionId { SessionId::from_raw(raw) }

    /// Build a distinct socket address for tests.
    fn address(port: u16) -> SocketAddr { format!("192.0.2.1:{port}").parse().unwrap() }

    #[test]
    fn unknown_addresses_are_not_attributed() {
        let map = AddressMap::new();
        assert_eq!(map.lookup(address(1000)), None);
        assert!(map.is_empty());
    }

    #[test]
    fn learned_addresses_are_returned() {
        let mut map = AddressMap::new();
        map.learn(address(1000), session(1));

        assert_eq!(map.lookup(address(1000)), Some(session(1)));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn learning_the_same_pair_twice_is_idempotent() {
        let mut map = AddressMap::new();
        map.learn(address(1000), session(1));
        map.learn(address(1000), session(1));

        assert_eq!(map.len(), 1);

        // The address must not have been recorded twice against the peer, or
        // forgetting the peer would leave a dangling entry behind.
        map.forget_session(session(1));
        assert!(map.is_empty());
    }

    #[test]
    fn one_peer_can_hold_several_addresses() {
        let mut map = AddressMap::new();
        map.learn(address(1000), session(1));
        map.learn(address(1001), session(1));

        assert_eq!(map.lookup(address(1000)), Some(session(1)));
        assert_eq!(map.lookup(address(1001)), Some(session(1)));
    }

    #[test]
    fn an_address_can_move_to_another_peer() {
        // A NAT rebinding can hand a previously seen address to a different peer.
        let mut map = AddressMap::new();
        map.learn(address(1000), session(1));
        map.learn(address(1000), session(2));

        assert_eq!(map.lookup(address(1000)), Some(session(2)));
        assert_eq!(map.len(), 1);

        // The address must no longer be attached to the original peer.
        map.forget_session(session(1));
        assert_eq!(map.lookup(address(1000)), Some(session(2)));
    }

    #[test]
    fn forgetting_a_stale_address_leaves_the_rest() {
        let mut map = AddressMap::new();
        map.learn(address(1000), session(1));
        map.learn(address(1001), session(1));

        map.forget_address(address(1000));

        assert_eq!(map.lookup(address(1000)), None);
        assert_eq!(map.lookup(address(1001)), Some(session(1)));
    }

    #[test]
    fn forgetting_a_peer_drops_all_its_addresses() {
        let mut map = AddressMap::new();
        map.learn(address(1000), session(1));
        map.learn(address(1001), session(1));
        map.learn(address(1002), session(2));

        map.forget_session(session(1));

        assert_eq!(map.lookup(address(1000)), None);
        assert_eq!(map.lookup(address(1001)), None);
        assert_eq!(map.lookup(address(1002)), Some(session(2)));
    }
}
