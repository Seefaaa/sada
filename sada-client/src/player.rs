//! The player ids this bridge hands the game.
//!
//! The voice server names players by whatever id the game gives it and never looks inside one, so how ids are made
//! is decided here. An id is derived from the ckey rather than counted out: the same ckey gets the same id after the
//! game or the server restarts, so a browser the server still holds bound to an id can never end up standing in for
//! somebody else.
//!
//! Deriving means two ckeys can land on one id. [`issue`] remembers who holds each id it gave out and refuses the
//! second ckey, which leaves that player undescribed rather than sharing an entry with the first.

use std::{
    cell::RefCell,
    collections::{HashMap, hash_map::Entry},
};

use sada_common::PlayerId;

/// Offset basis of the 64-bit FNV-1a hash.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Prime of the 64-bit FNV-1a hash.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

thread_local! {
    /// The ckey holding each id handed out so far.
    ///
    /// Kept apart from the control connection, so ids can be issued before `init` and stay put when the game starts
    /// the worker again.
    static ISSUED: RefCell<HashMap<PlayerId, String>> = RefCell::new(HashMap::new());
}

/// Derive the id for `ckey`: FNV-1a over its bytes, folded to 32 bits.
fn derive(ckey: &str) -> PlayerId {
    let mut hash = FNV_OFFSET;

    for byte in ckey.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }

    PlayerId::from_raw(((hash >> 32) as u32) ^ (hash as u32))
}

/// The id for `ckey`, or `None` when a different ckey already holds the id it derives.
pub fn issue(ckey: &str) -> Option<PlayerId> {
    let player = derive(ckey);

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

#[cfg(test)]
mod tests {
    use sada_common::PlayerId;

    use super::{derive, forget, issue};

    /// Two ckeys that derive the same id, found by searching over the derivation.
    const COLLIDING: (&str, &str) = ("zxboiwrq", "iqbltqzu");

    #[test]
    fn a_ckey_always_derives_the_same_id() {
        // What makes a derived id safe across restarts is that it does not move. These pin the function, so a change
        // to it is a decision rather than an accident.
        for (ckey, expected) in [
            ("sefa", 0xfef1_9f30),
            ("adminbus", 0x54b3_c707),
            ("other", 0xc872_8734),
            ("ghost", 0xe61d_4128),
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
        assert_eq!(
            derive(COLLIDING.0),
            derive(COLLIDING.1),
            "the pair this test is built on stopped colliding"
        );

        assert!(issue(COLLIDING.0).is_some());
        assert_eq!(issue(COLLIDING.1), None, "the second ckey has to be refused");
        assert!(issue(COLLIDING.0).is_some(), "the first one keeps its id");
    }

    #[test]
    fn forgetting_a_player_frees_their_id() {
        let first = issue(COLLIDING.0).expect("the first ckey");

        forget(first);

        assert_eq!(issue(COLLIDING.1), Some(first), "the id is free once its holder leaves");
        assert_eq!(issue(COLLIDING.0), None, "and now it is the first one that is refused");
    }
}
