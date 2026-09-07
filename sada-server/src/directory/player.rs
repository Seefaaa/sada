//! Accumulated per-player state, as described by the game server.
//!
//! The game sends deltas ([`PlayerPatch`]), so the authoritative picture lives here and each patch is merged into it.

use std::collections::{HashMap, hash_map::Entry};

use sada_common::{Ckey, Freq, PlayerPatch, Position};

/// Everything the game has told us about one player.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerState {
    /// Whether the game forbids this player from speaking.
    pub mute: bool,
    /// Whether the game forbids this player from hearing.
    pub deaf: bool,
    /// Whether the player holds admin rights.
    pub is_admin: bool,
    /// Whether a dead player has been granted omnipresent hearing.
    pub ghost_ears: bool,
    /// Where the player is standing, when the game reports positions.
    pub position: Option<Position>,
    /// Players who can hear this one speak locally, when the game computes it.
    pub local_with: Vec<Ckey>,
    /// Frequencies this player may transmit on.
    pub hot_freqs: Vec<Freq>,
    /// Frequencies this player receives.
    pub hear_freqs: Vec<Freq>,
    /// Languages this player understands.
    pub known_languages: Vec<String>,
    /// Language this player is currently speaking.
    pub current_language: Option<String>,
}

impl Default for PlayerState {
    /// A player the game has not described yet can neither speak nor hear.
    ///
    /// Failing closed matters here: a player who is wrongly silent is an obvious bug, while a player who is wrongly
    /// audible leaks speech that the game intended to be private. The game is expected to send a complete patch when a
    /// player first appears and after a server restart.
    fn default() -> Self {
        Self {
            mute: true,
            deaf: true,
            is_admin: false,
            ghost_ears: false,
            position: None,
            local_with: Vec::new(),
            hot_freqs: Vec::new(),
            hear_freqs: Vec::new(),
            known_languages: Vec::new(),
            current_language: None,
        }
    }
}

impl PlayerState {
    /// Fold a delta into this state, leaving absent fields untouched.
    pub fn apply(&mut self, patch: PlayerPatch) {
        let PlayerPatch {
            mute,
            deaf,
            is_admin,
            ghost_ears,
            position,
            local_with,
            hot_freqs,
            hear_freqs,
            known_languages,
            current_language,
        } = patch;

        if let Some(mute) = mute {
            self.mute = mute;
        }
        if let Some(deaf) = deaf {
            self.deaf = deaf;
        }
        if let Some(is_admin) = is_admin {
            self.is_admin = is_admin;
        }
        if let Some(ghost_ears) = ghost_ears {
            self.ghost_ears = ghost_ears;
        }
        if let Some(position) = position {
            self.position = Some(position);
        }
        if let Some(local_with) = local_with {
            self.local_with = local_with;
        }
        if let Some(hot_freqs) = hot_freqs {
            self.hot_freqs = hot_freqs;
        }
        if let Some(hear_freqs) = hear_freqs {
            self.hear_freqs = hear_freqs;
        }
        if let Some(known_languages) = known_languages {
            self.known_languages = known_languages;
        }
        if let Some(current_language) = current_language {
            self.current_language = Some(current_language);
        }
    }

    /// Whether this player can be heard by anyone at all.
    #[must_use]
    pub fn can_speak(&self) -> bool { !self.mute }

    /// Whether this player can hear anyone at all.
    #[must_use]
    pub fn can_hear(&self) -> bool { !self.deaf }
}

/// Every player the game has described.
#[derive(Debug, Default)]
pub struct PlayerTable {
    /// State by player key.
    players: HashMap<Ckey, PlayerState>,
}

impl PlayerTable {
    /// Create an empty table.
    #[must_use]
    pub fn new() -> Self { Self::default() }

    /// Fold a delta into a player's state, creating it if unknown.
    ///
    /// Returns whether anything actually changed, so the caller can skip recomputing the routing table for a no-op
    /// patch.
    pub fn apply(&mut self, ckey: Ckey, patch: PlayerPatch) -> bool {
        match self.players.entry(ckey) {
            Entry::Occupied(mut entry) => {
                let before = entry.get().clone();
                entry.get_mut().apply(patch);
                *entry.get() != before
            },
            Entry::Vacant(entry) => {
                let mut state = PlayerState::default();
                state.apply(patch);
                entry.insert(state);
                true
            },
        }
    }

    /// Forget a player entirely. Returns whether they were known.
    pub fn remove(&mut self, ckey: &Ckey) -> bool { self.players.remove(ckey).is_some() }

    /// Look up a player's state.
    #[must_use]
    pub fn get(&self, ckey: &Ckey) -> Option<&PlayerState> { self.players.get(ckey) }

    /// Iterate over every known player.
    pub fn iter(&self) -> impl Iterator<Item = (&Ckey, &PlayerState)> { self.players.iter() }

    /// Number of known players.
    #[must_use]
    pub fn len(&self) -> usize { self.players.len() }

    /// Whether the game has described nobody yet.
    #[cfg(test)]
    fn is_empty(&self) -> bool { self.players.is_empty() }
}

#[cfg(test)]
mod tests {
    use sada_common::{Freq, PlayerPatch, Position};

    use super::{PlayerState, PlayerTable};

    #[test]
    fn an_undescribed_player_is_silent_and_deaf() {
        let state = PlayerState::default();
        assert!(!state.can_speak());
        assert!(!state.can_hear());
    }

    #[test]
    fn a_patch_only_touches_the_fields_it_carries() {
        let mut state = PlayerState::default();
        state.apply(PlayerPatch {
            mute: Some(false),
            deaf: Some(false),
            hear_freqs: Some(vec![Freq(1459)]),
            ..Default::default()
        });

        state.apply(PlayerPatch {
            position: Some(Position { x: 1, y: 2, z: 3 }),
            ..Default::default()
        });

        assert!(state.can_speak());
        assert!(state.can_hear());
        assert_eq!(state.hear_freqs, vec![Freq(1459)]);
        assert_eq!(state.position, Some(Position { x: 1, y: 2, z: 3 }));
    }

    #[test]
    fn a_list_field_is_replaced_not_merged() {
        let mut state = PlayerState::default();
        state.apply(PlayerPatch {
            hear_freqs: Some(vec![Freq(1459), Freq(1351)]),
            ..Default::default()
        });
        state.apply(PlayerPatch {
            hear_freqs: Some(vec![Freq(1351)]),
            ..Default::default()
        });

        assert_eq!(state.hear_freqs, vec![Freq(1351)]);
    }

    #[test]
    fn an_unknown_player_is_created_by_their_first_patch() {
        let mut table = PlayerTable::new();
        assert!(table.apply(
            "sefa".into(),
            PlayerPatch {
                mute: Some(false),
                ..Default::default()
            }
        ));

        assert_eq!(table.len(), 1);
        assert!(table.get(&"sefa".into()).unwrap().can_speak());
        assert!(!table.get(&"sefa".into()).unwrap().can_hear());
    }

    #[test]
    fn a_patch_that_changes_nothing_reports_no_change() {
        let mut table = PlayerTable::new();
        table.apply(
            "sefa".into(),
            PlayerPatch {
                deaf: Some(false),
                ..Default::default()
            },
        );

        assert!(!table.apply(
            "sefa".into(),
            PlayerPatch {
                deaf: Some(false),
                ..Default::default()
            }
        ));
        assert!(!table.apply("sefa".into(), PlayerPatch::default()));
    }

    #[test]
    fn removing_a_player_forgets_them() {
        let mut table = PlayerTable::new();
        table.apply("sefa".into(), PlayerPatch::default());

        assert!(table.remove(&"sefa".into()));
        assert!(!table.remove(&"sefa".into()));
        assert!(table.is_empty());
    }
}
