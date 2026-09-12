//! The auth codes the game mints, and how long they last.
//!
//! A code is the only thing tying a browser to a player, so the rules about when one stops working are worth keeping
//! in one place. A player holds at most one live code, and it dies when a browser redeems it, when they ask for
//! another one, when they leave, or when it expires.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use sada_common::{AuthCode, Ckey};

/// A code the game minted, waiting for a browser to present it.
struct PendingCode {
    /// Player the code identifies.
    ckey: Ckey,
    /// When it stops being accepted.
    expires: Instant,
}

/// Codes the game has minted that nobody has connected with yet.
pub struct CodeTable {
    /// The codes themselves.
    codes: HashMap<AuthCode, PendingCode>,
    /// How long a freshly minted code stays valid.
    ttl: Duration,
}

impl CodeTable {
    /// An empty table whose codes live for `ttl`.
    pub fn new(ttl: Duration) -> Self {
        Self {
            codes: HashMap::new(),
            ttl,
        }
    }

    /// Record a code the game minted for a player, retiring the one they had.
    ///
    /// The game mints a fresh code every time the player asks and shows it in a window that replaces the last one, so
    /// the older code is already gone from the only place the player could read it. Keeping it alive here would mean
    /// a code that opens a player's session and that nobody is looking at any more.
    pub fn register(&mut self, code: AuthCode, ckey: Ckey) {
        self.spend(&ckey);
        self.register_at(code, ckey, Instant::now() + self.ttl);
    }

    /// Record a code that stops working at a given moment.
    fn register_at(&mut self, code: AuthCode, ckey: Ckey, expires: Instant) {
        self.codes.insert(code, PendingCode { ckey, expires });
    }

    /// Spend a code, and say who it belonged to.
    ///
    /// Redeeming consumes: whoever gets `Some` here is the one browser that code will ever authenticate, so two
    /// browsers racing on one code end with the second refused at its handshake, and a browser still holding a code
    /// the player has since replaced is refused too.
    pub fn redeem(&mut self, code: &AuthCode) -> Option<Ckey> {
        let pending = self.codes.remove(code)?;
        (pending.expires > Instant::now()).then_some(pending.ckey)
    }

    /// Drop the code a player is holding, because they left or asked for another one.
    pub fn spend(&mut self, ckey: &Ckey) { self.codes.retain(|_, pending| &pending.ckey != ckey); }

    /// Drop the codes nobody came back for, and say how many that was.
    pub fn sweep(&mut self) -> usize {
        let now = Instant::now();
        let before = self.codes.len();

        self.codes.retain(|_, pending| pending.expires > now);

        before - self.codes.len()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use sada_common::{AuthCode, Ckey};

    use super::CodeTable;

    /// A table whose codes are still good, and one code already in it.
    fn table() -> (CodeTable, AuthCode, Ckey) {
        let mut codes = CodeTable::new(Duration::from_secs(300));
        let code = AuthCode::from("ABC123");
        let ckey = Ckey::from("adminbus");

        codes.register(code.clone(), ckey.clone());

        (codes, code, ckey)
    }

    #[test]
    fn a_registered_code_resolves_to_its_player() {
        let (mut codes, code, ckey) = table();
        assert_eq!(codes.redeem(&code), Some(ckey));
    }

    #[test]
    fn an_unknown_code_resolves_to_nobody() {
        let (mut codes, ..) = table();
        assert_eq!(codes.redeem(&AuthCode::from("NOPE99")), None);
    }

    #[test]
    fn a_code_can_only_be_redeemed_once() {
        let (mut codes, code, ckey) = table();

        // Two browsers racing on one code: the second one is turned away.
        assert_eq!(codes.redeem(&code), Some(ckey));
        assert_eq!(codes.redeem(&code), None);
    }

    #[test]
    fn minting_a_new_code_retires_the_old_one() {
        let (mut codes, code, ckey) = table();
        let second = AuthCode::from("XYZ789");

        codes.register(second.clone(), ckey.clone());

        assert_eq!(codes.redeem(&code), None);
        assert_eq!(codes.redeem(&second), Some(ckey));
    }

    #[test]
    fn leaving_spends_the_players_code() {
        let (mut codes, code, ckey) = table();

        codes.spend(&ckey);

        assert_eq!(codes.redeem(&code), None);
    }

    #[test]
    fn one_player_leaving_leaves_another_player_alone() {
        let (mut codes, code, ckey) = table();

        codes.spend(&Ckey::from("somebodyelse"));

        assert_eq!(codes.redeem(&code), Some(ckey));
    }

    #[test]
    fn an_expired_code_is_refused_and_forgotten() {
        let (mut codes, code, ckey) = table();
        codes.register_at(code.clone(), ckey, Instant::now());

        assert_eq!(codes.redeem(&code), None);
        assert_eq!(
            codes.sweep(),
            0,
            "refusing an expired code should already have dropped it"
        );
    }

    #[test]
    fn the_sweep_drops_expired_codes_and_keeps_the_rest() {
        let (mut codes, code, ckey) = table();

        let stale = AuthCode::from("OLD123");
        codes.register_at(stale.clone(), Ckey::from("ghost"), Instant::now());

        assert_eq!(codes.sweep(), 1);
        assert_eq!(codes.redeem(&stale), None);
        assert_eq!(codes.redeem(&code), Some(ckey));
    }
}
