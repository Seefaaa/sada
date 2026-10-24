//! The player ids this bridge hands the game.
//!
//! The voice server names players by whatever id the game gives it and never looks inside one, so how ids are made
//! is decided here. An id is derived from the ckey rather than counted out: the same ckey gets the same id after the
//! game or the server restarts, so a browser the server still holds bound to an id can never end up standing in for
//! somebody else.
//!
//! The derivation is a cryptographic hash, so nobody can pick a ckey that lands on somebody else's id. Two ckeys can
//! still meet by chance, however unlikely that is over 64 bits. [`issue`] remembers who holds each id it gave out and
//! refuses the second ckey, which leaves that player undescribed rather than sharing an entry with the first.

use std::{
    cell::RefCell,
    collections::{HashMap, hash_map::Entry},
};

use sada_common::PlayerId;

thread_local! {
    /// The ckey holding each id handed out so far.
    ///
    /// Kept apart from the control connection, so ids can be issued before `init` and stay put when the game starts
    /// the worker again.
    static ISSUED: RefCell<HashMap<PlayerId, String>> = RefCell::new(HashMap::new());
}

/// Derive the id for `ckey`: the first eight bytes of its BLAKE3 hash, little-endian.
fn derive(ckey: &str) -> PlayerId {
    let mut bytes = [0; size_of::<u64>()];

    blake3::Hasher::new()
        .update(ckey.as_bytes())
        .finalize_xof()
        .fill(&mut bytes);

    PlayerId::from_raw(u64::from_le_bytes(bytes))
}

/// The id for `ckey`, or `None` when a different ckey already holds the id it derives.
pub fn issue(ckey: &str) -> Option<PlayerId> { claim(ckey, derive(ckey)) }

/// Give `player` to `ckey`, or `None` when a different ckey already holds it.
pub(crate) fn claim(ckey: &str, player: PlayerId) -> Option<PlayerId> {
    ISSUED.with_borrow_mut(|issued| match issued.entry(player) {
        Entry::Occupied(holder) => (holder.get() == ckey).then_some(player),
        Entry::Vacant(slot) => {
            slot.insert(ckey.to_owned());
            Some(player)
        },
    })
}

/// Free the id `player` was issued, because the game removed them.
pub fn forget(player: PlayerId) { ISSUED.with_borrow_mut(|issued| issued.remove(&player)); }

/// Issue one id to two ckeys in turn, as if both derived it, returning what each was given.
///
/// The collision is made up.
#[cfg(test)]
pub(crate) fn collide(first: &str, second: &str) -> (Option<PlayerId>, Option<PlayerId>) {
    let shared = derive(first);
    (claim(first, shared), claim(second, shared))
}

#[cfg(test)]
mod tests {
    use sada_common::PlayerId;

    use super::{claim, collide, derive, forget, issue};

    #[test]
    fn a_ckey_always_derives_the_same_id() {
        // What makes a derived id safe across restarts is that it does not move. These pin the function, so a change
        // to it is a decision rather than an accident.
        for (ckey, expected) in [
            ("sefa", 0xa17a_d862_033f_90ce),
            ("adminbus", 0x209a_9e2b_7aa0_99d9),
            ("dave", 0x0746_ea1e_cf20_ac67),
            ("ghost", 0xd9ee_be45_a1fd_d17b),
        ] {
            assert_eq!(derive(ckey), PlayerId::from_raw(expected), "{ckey}");
        }
    }

    #[test]
    fn the_same_ckey_is_issued_the_same_id_again() {
        assert_eq!(issue("sefa"), Some(derive("sefa")));
        assert_eq!(issue("sefa"), Some(derive("sefa")));
        assert_ne!(issue("sefa"), issue("other"));
    }

    #[test]
    fn two_ckeys_deriving_one_id_do_not_become_one_player() {
        let (first, second) = collide("sefa", "other");

        assert!(first.is_some());
        assert_eq!(second, None, "the second ckey has to be refused");
        assert_eq!(issue("sefa"), first, "the first one keeps its id");
    }

    #[test]
    fn forgetting_a_player_frees_their_id() {
        let (first, _) = collide("sefa", "other");
        let first = first.expect("the first ckey");

        forget(first);

        assert_eq!(
            claim("other", first),
            Some(first),
            "the id is free once its holder leaves"
        );
        assert_eq!(issue("sefa"), None, "and now it is the first one that is refused");
    }
}
