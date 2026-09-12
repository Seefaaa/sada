//! Who can hear whom.
//!
//! Routing is computed ahead of time and handed to the SFU as a snapshot, so relaying a frame costs a lookup rather
//! than a query across tasks.
//!
//! The table deliberately does *not* encode push-to-talk. It answers "who could hear this player if they spoke", split
//! by local speech and by radio channel; which of those applies right now is decided by the speaker's current transmit
//! intent, which arrives out of band and is applied immediately.

use std::collections::{HashMap, HashSet};

use sada_common::{Ckey, Freq, Position};

use super::player::PlayerState;
use crate::directory::player::PlayerTable;

/// Empty listener list, returned for speakers nobody can hear.
const NO_LISTENERS: &[Ckey] = &[];

/// The routing policy currently in force.
#[derive(Debug, Default)]
pub enum Routing {
    /// Everyone hears everyone else.
    ///
    /// This is the state before the game has said anything, and the policy used
    /// when running standalone without a game server.
    #[default]
    Unrestricted,
    /// Explicit listener sets derived from game state.
    Explicit(RoutingTable),
}

/// Precomputed listener sets.
#[derive(Debug, Default, PartialEq)]
pub struct RoutingTable {
    /// Players the game currently permits to speak.
    speakers: HashSet<Ckey>,
    /// Listeners for local speech, by speaker.
    local: HashMap<Ckey, Vec<Ckey>>,
    /// Listeners tuned to each radio frequency.
    ///
    /// Radio reception does not depend on who is transmitting, so this is keyed
    /// by frequency alone; the speaker is excluded when the set is read.
    radio: HashMap<Freq, Vec<Ckey>>,
    /// Frequencies each speaker is allowed to transmit on.
    hot: HashMap<Ckey, Vec<Freq>>,
}

impl RoutingTable {
    /// Whether the game permits `speaker` to be heard at all.
    #[must_use]
    pub fn can_speak(&self, speaker: &Ckey) -> bool { self.speakers.contains(speaker) }

    /// Players who hear `speaker` talking locally.
    #[must_use]
    pub fn local_listeners(&self, speaker: &Ckey) -> &[Ckey] {
        self.local.get(speaker).map_or(NO_LISTENERS, Vec::as_slice)
    }

    /// Players tuned to `channel`.
    ///
    /// The speaker may appear in the result and is filtered out by the caller,
    /// which already skips relaying a frame back to its origin.
    #[must_use]
    pub fn radio_listeners(&self, channel: Freq) -> &[Ckey] {
        self.radio.get(&channel).map_or(NO_LISTENERS, Vec::as_slice)
    }

    /// Whether `speaker` may transmit on `channel`.
    ///
    /// Being able to hear a channel does not grant the right to talk on it, so
    /// this is checked before relaying radio speech rather than trusting the
    /// frequency the transmit intent names.
    #[must_use]
    pub fn can_transmit_on(&self, speaker: &Ckey, channel: Freq) -> bool {
        self.hot.get(speaker).is_some_and(|hot| hot.contains(&channel))
    }
}

/// Turns game state into a routing table.
///
/// Implementations differ only in how local audibility is decided; radio routing and
/// speech permission are the same for all of them.
pub trait Router: Send + Sync {
    /// Short name used in logs and configuration.
    fn name(&self) -> &'static str;

    /// Build the routing snapshot for the current game state.
    fn compute(&self, players: &PlayerTable) -> Routing;
}

/// Everyone hears everyone.
#[derive(Debug, Default)]
pub struct BroadcastRouter;

impl Router for BroadcastRouter {
    fn name(&self) -> &'static str { "broadcast" }

    fn compute(&self, _players: &PlayerTable) -> Routing { Routing::Unrestricted }
}

/// Local audibility as computed by the game.
///
/// The game is the only thing that knows about walls, doors, holopads and whispering, so it sends the hearer list
/// directly and the server just relays along it. This is the policy that matches the game's own rules exactly.
#[derive(Debug, Default)]
pub struct HearerListRouter;

impl Router for HearerListRouter {
    fn name(&self) -> &'static str { "hearer-list" }

    fn compute(&self, players: &PlayerTable) -> Routing {
        let mut table = base_table(players);

        for (ckey, state) in players.iter() {
            if !state.can_speak() {
                continue;
            }

            let listeners = state
                .local_with
                .iter()
                .filter(|listener| *listener != ckey && can_hear(players, listener))
                .cloned()
                .collect::<Vec<_>>();

            if !listeners.is_empty() {
                table.local.insert(ckey.clone(), listeners);
            }
        }

        Routing::Explicit(table)
    }
}

/// Local audibility from raw coordinates.
///
/// Cheaper for the game, it only reports positions, but the server has no idea about walls, so sound carries through
/// them. Useful where the game cannot afford to compute hearer sets.
#[derive(Debug)]
pub struct ProximityRouter {
    /// Maximum distance, in tiles, at which local speech is audible.
    pub radius: i32,
}

impl Default for ProximityRouter {
    /// Matches the game's default hearing range.
    fn default() -> Self { Self { radius: 7 } }
}

impl ProximityRouter {
    /// Whether two positions are within earshot.
    ///
    /// Distance is measured the way the game does it: Chebyshev on the tile grid, and never across z-levels.
    fn in_range(&self, from: Position, to: Position) -> bool {
        from.z == to.z && (from.x - to.x).abs().max((from.y - to.y).abs()) <= self.radius
    }
}

impl Router for ProximityRouter {
    fn name(&self) -> &'static str { "proximity" }

    fn compute(&self, players: &PlayerTable) -> Routing {
        let mut table = base_table(players);

        for (ckey, state) in players.iter() {
            let (true, Some(origin)) = (state.can_speak(), state.position) else {
                continue;
            };

            let listeners = players
                .iter()
                .filter(|(listener, other)| {
                    *listener != ckey
                        && other.can_hear()
                        && other.position.is_some_and(|there| self.in_range(origin, there))
                })
                .map(|(listener, _)| listener.clone())
                .collect::<Vec<_>>();

            if !listeners.is_empty() {
                table.local.insert(ckey.clone(), listeners);
            }
        }

        Routing::Explicit(table)
    }
}

/// Build the parts of the table that every policy shares.
///
/// Radio reception and speech permission come straight from game state and do not depend on how local audibility is
/// decided.
fn base_table(players: &PlayerTable) -> RoutingTable {
    let mut speakers = HashSet::new();
    let mut radio: HashMap<Freq, Vec<Ckey>> = HashMap::new();
    let mut hot: HashMap<Ckey, Vec<Freq>> = HashMap::new();

    for (ckey, state) in players.iter() {
        if state.can_speak() {
            speakers.insert(ckey.clone());
            if !state.hot_freqs.is_empty() {
                hot.insert(ckey.clone(), state.hot_freqs.clone());
            }
        }

        if !state.can_hear() {
            continue;
        }

        for &channel in &state.hear_freqs {
            radio.entry(channel).or_default().push(ckey.clone());
        }
    }

    RoutingTable {
        speakers,
        local: HashMap::new(),
        radio,
        hot,
    }
}

/// Whether a player exists and is able to hear.
fn can_hear(players: &PlayerTable, ckey: &Ckey) -> bool { players.get(ckey).is_some_and(PlayerState::can_hear) }

#[cfg(test)]
mod tests {
    use sada_common::{Ckey, Freq, PlayerPatch, Position};

    use super::{BroadcastRouter, HearerListRouter, ProximityRouter, Router, Routing, RoutingTable};
    use crate::directory::player::PlayerTable;

    /// Build a table from a list of players and their patches.
    fn table_of(players: &[(&str, PlayerPatch)]) -> PlayerTable {
        let mut table = PlayerTable::new();
        for (ckey, patch) in players {
            table.apply(Ckey::from(*ckey), patch.clone());
        }
        table
    }

    /// A player who can both speak and hear.
    fn present() -> PlayerPatch {
        PlayerPatch {
            mute: Some(false),
            deaf: Some(false),
            ..Default::default()
        }
    }

    /// Unwrap an explicit routing table.
    fn explicit(routing: Routing) -> RoutingTable {
        match routing {
            Routing::Explicit(table) => table,
            Routing::Unrestricted => panic!("expected an explicit table"),
        }
    }

    /// Listener list as owned strings, sorted for comparison.
    fn sorted(listeners: &[Ckey]) -> Vec<String> {
        let mut names = listeners.iter().map(ToString::to_string).collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn broadcast_ignores_game_state_entirely() {
        let players = table_of(&[("a", present())]);
        assert!(matches!(BroadcastRouter.compute(&players), Routing::Unrestricted));
    }

    #[test]
    fn hearer_list_relays_along_the_game_s_own_list() {
        let players = table_of(&[
            (
                "speaker",
                PlayerPatch {
                    local_with: Some(vec!["near".into()]),
                    ..present()
                },
            ),
            ("near", present()),
            ("far", present()),
        ]);

        let table = explicit(HearerListRouter.compute(&players));

        assert_eq!(sorted(table.local_listeners(&"speaker".into())), ["near"]);
        assert!(table.local_listeners(&"far".into()).is_empty());
    }

    #[test]
    fn a_muted_speaker_reaches_nobody() {
        let players = table_of(&[
            (
                "speaker",
                PlayerPatch {
                    mute: Some(true),
                    deaf: Some(false),
                    local_with: Some(vec!["near".into()]),
                    ..Default::default()
                },
            ),
            ("near", present()),
        ]);

        let table = explicit(HearerListRouter.compute(&players));

        assert!(!table.can_speak(&"speaker".into()));
        assert!(table.local_listeners(&"speaker".into()).is_empty());
    }

    #[test]
    fn a_deaf_listener_is_dropped_from_the_list() {
        let players = table_of(&[
            (
                "speaker",
                PlayerPatch {
                    local_with: Some(vec!["deafened".into(), "hearing".into()]),
                    ..present()
                },
            ),
            (
                "deafened",
                PlayerPatch {
                    deaf: Some(true),
                    ..present()
                },
            ),
            ("hearing", present()),
        ]);

        let table = explicit(HearerListRouter.compute(&players));

        assert_eq!(sorted(table.local_listeners(&"speaker".into())), ["hearing"]);
    }

    #[test]
    fn a_speaker_is_never_their_own_listener() {
        let players = table_of(&[(
            "speaker",
            PlayerPatch {
                local_with: Some(vec!["speaker".into()]),
                ..present()
            },
        )]);

        let table = explicit(HearerListRouter.compute(&players));

        assert!(table.local_listeners(&"speaker".into()).is_empty());
    }

    #[test]
    fn radio_listeners_are_keyed_by_frequency() {
        let players = table_of(&[
            (
                "speaker",
                PlayerPatch {
                    hot_freqs: Some(vec![Freq(1459)]),
                    ..present()
                },
            ),
            (
                "common",
                PlayerPatch {
                    hear_freqs: Some(vec![Freq(1459)]),
                    ..present()
                },
            ),
            (
                "security",
                PlayerPatch {
                    hear_freqs: Some(vec![Freq(1359)]),
                    ..present()
                },
            ),
        ]);

        let table = explicit(HearerListRouter.compute(&players));

        assert_eq!(sorted(table.radio_listeners(Freq(1459))), ["common"]);
        assert_eq!(sorted(table.radio_listeners(Freq(1359))), ["security"]);
        assert!(table.radio_listeners(Freq(1)).is_empty());
    }

    #[test]
    fn transmitting_needs_the_frequency_to_be_hot() {
        let players = table_of(&[(
            "speaker",
            PlayerPatch {
                hot_freqs: Some(vec![Freq(1459)]),
                ..present()
            },
        )]);

        let table = explicit(HearerListRouter.compute(&players));

        assert!(table.can_transmit_on(&"speaker".into(), Freq(1459)));
        // Listening to a channel does not grant the right to talk on it.
        assert!(!table.can_transmit_on(&"speaker".into(), Freq(1359)));
    }

    #[test]
    fn a_deaf_player_hears_no_radio() {
        let players = table_of(&[(
            "deafened",
            PlayerPatch {
                deaf: Some(true),
                hear_freqs: Some(vec![Freq(1459)]),
                ..present()
            },
        )]);

        let table = explicit(HearerListRouter.compute(&players));

        assert!(table.radio_listeners(Freq(1459)).is_empty());
    }

    #[test]
    fn proximity_uses_chebyshev_distance() {
        let router = ProximityRouter { radius: 7 };
        let players = table_of(&[
            (
                "speaker",
                PlayerPatch {
                    position: Some(Position { x: 0, y: 0, z: 1 }),
                    ..present()
                },
            ),
            // Diagonally 7 away: inside a Chebyshev radius, outside a Euclidean one.
            (
                "corner",
                PlayerPatch {
                    position: Some(Position { x: 7, y: 7, z: 1 }),
                    ..present()
                },
            ),
            (
                "outside",
                PlayerPatch {
                    position: Some(Position { x: 8, y: 0, z: 1 }),
                    ..present()
                },
            ),
        ]);

        let table = explicit(router.compute(&players));

        assert_eq!(sorted(table.local_listeners(&"speaker".into())), ["corner"]);
    }

    #[test]
    fn proximity_never_carries_between_z_levels() {
        let players = table_of(&[
            (
                "speaker",
                PlayerPatch {
                    position: Some(Position { x: 0, y: 0, z: 1 }),
                    ..present()
                },
            ),
            (
                "upstairs",
                PlayerPatch {
                    position: Some(Position { x: 0, y: 0, z: 2 }),
                    ..present()
                },
            ),
        ]);

        let table = explicit(ProximityRouter::default().compute(&players));

        assert!(table.local_listeners(&"speaker".into()).is_empty());
    }

    #[test]
    fn proximity_ignores_players_with_no_known_position() {
        let players = table_of(&[
            (
                "speaker",
                PlayerPatch {
                    position: Some(Position { x: 0, y: 0, z: 1 }),
                    ..present()
                },
            ),
            ("nowhere", present()),
        ]);

        let table = explicit(ProximityRouter::default().compute(&players));

        assert!(table.local_listeners(&"speaker".into()).is_empty());
    }
}
