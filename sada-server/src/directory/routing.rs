//! Who can hear whom.
//!
//! Routing is computed ahead of time and handed to the SFU as a snapshot, so relaying a frame costs a lookup rather
//! than a query across tasks.
//!
//! The table deliberately does *not* encode push-to-talk. It answers "who could hear this player if they spoke", split
//! by local speech and by radio channel; which of those applies right now is decided by the speaker's current transmit
//! intent, which arrives out of band and is applied immediately.

use rustc_hash::{FxHashMap, FxHashSet};
use sada_common::{Freq, PlayerId, Position};

use super::player::PlayerState;
use crate::directory::player::PlayerTable;

/// Empty listener list, returned for speakers nobody can hear.
const NO_LISTENERS: &[PlayerId] = &[];

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
    speakers: FxHashSet<PlayerId>,
    /// Listeners for local speech, by speaker.
    local: FxHashMap<PlayerId, Vec<PlayerId>>,
    /// Listeners tuned to each radio frequency.
    ///
    /// Radio reception does not depend on who is transmitting, so this is keyed
    /// by frequency alone; the speaker is excluded when the set is read.
    radio: FxHashMap<Freq, Vec<PlayerId>>,
    /// Frequencies each speaker is allowed to transmit on.
    hot: FxHashMap<PlayerId, Vec<Freq>>,
    /// Where each player the game has placed is standing.
    positions: FxHashMap<PlayerId, Position>,
}

impl RoutingTable {
    /// Whether the game permits `speaker` to be heard at all.
    #[must_use]
    pub fn can_speak(&self, speaker: PlayerId) -> bool { self.speakers.contains(&speaker) }

    /// Players who hear `speaker` talking locally.
    #[must_use]
    pub fn local_listeners(&self, speaker: PlayerId) -> &[PlayerId] {
        self.local.get(&speaker).map_or(NO_LISTENERS, Vec::as_slice)
    }

    /// Players tuned to `channel`.
    ///
    /// The speaker may appear in the result and is filtered out by the caller,
    /// which already skips relaying a frame back to its origin.
    #[must_use]
    pub fn radio_listeners(&self, channel: Freq) -> &[PlayerId] {
        self.radio.get(&channel).map_or(NO_LISTENERS, Vec::as_slice)
    }

    /// Whether `speaker` may transmit on `channel`.
    ///
    /// Being able to hear a channel does not grant the right to talk on it, so
    /// this is checked before relaying radio speech rather than trusting the
    /// frequency the transmit intent names.
    #[must_use]
    pub fn can_transmit_on(&self, speaker: PlayerId, channel: Freq) -> bool {
        self.hot.get(&speaker).is_some_and(|hot| hot.contains(&channel))
    }

    /// Where a player is standing, if the game has said.
    #[must_use]
    pub fn position(&self, player: PlayerId) -> Option<Position> { self.positions.get(&player).copied() }
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

        for (player, state) in players.iter() {
            if !state.can_speak() {
                continue;
            }

            let listeners = state
                .local_with
                .iter()
                .copied()
                .filter(|listener| *listener != player && can_hear(players, *listener))
                .collect::<Vec<_>>();

            if !listeners.is_empty() {
                table.local.insert(player, listeners);
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

        for (player, state) in players.iter() {
            let (true, Some(origin)) = (state.can_speak(), state.position) else {
                continue;
            };

            let listeners = players
                .iter()
                .filter(|(listener, other)| {
                    *listener != player
                        && other.can_hear()
                        && other.position.is_some_and(|there| self.in_range(origin, there))
                })
                .map(|(listener, _)| listener)
                .collect::<Vec<_>>();

            if !listeners.is_empty() {
                table.local.insert(player, listeners);
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
    let mut speakers = FxHashSet::default();
    let mut radio: FxHashMap<Freq, Vec<PlayerId>> = FxHashMap::default();
    let mut hot = FxHashMap::default();
    let mut positions = FxHashMap::default();

    for (player, state) in players.iter() {
        if let Some(position) = state.position {
            positions.insert(player, position);
        }

        if state.can_speak() {
            speakers.insert(player);
            if !state.hot_freqs.is_empty() {
                hot.insert(player, state.hot_freqs.clone());
            }
        }

        if !state.can_hear() {
            continue;
        }

        for &channel in &state.hear_freqs {
            radio.entry(channel).or_default().push(player);
        }
    }

    RoutingTable {
        speakers,
        local: FxHashMap::default(),
        radio,
        hot,
        positions,
    }
}

/// Whether a player exists and is able to hear.
fn can_hear(players: &PlayerTable, player: PlayerId) -> bool { players.get(player).is_some_and(PlayerState::can_hear) }

#[cfg(test)]
mod tests {
    use sada_common::{Freq, PlayerId, PlayerPatch, Position};

    use super::{BroadcastRouter, HearerListRouter, ProximityRouter, Router, Routing, RoutingTable};
    use crate::directory::player::PlayerTable;

    /// A player in the fixtures below.
    const ANYONE: PlayerId = PlayerId::from_raw(1);

    /// A player in the fixtures below.
    const SPEAKER: PlayerId = PlayerId::from_raw(2);

    /// A player in the fixtures below.
    const NEAR: PlayerId = PlayerId::from_raw(3);

    /// A player in the fixtures below.
    const FAR: PlayerId = PlayerId::from_raw(4);

    /// A player in the fixtures below.
    const DEAFENED: PlayerId = PlayerId::from_raw(5);

    /// A player in the fixtures below.
    const HEARING: PlayerId = PlayerId::from_raw(6);

    /// A player in the fixtures below.
    const ASSISTANT: PlayerId = PlayerId::from_raw(7);

    /// A player in the fixtures below.
    const SECURITY_OFFICER: PlayerId = PlayerId::from_raw(8);

    /// A player in the fixtures below.
    const CORNER: PlayerId = PlayerId::from_raw(9);

    /// A player in the fixtures below.
    const OUTSIDE: PlayerId = PlayerId::from_raw(10);

    /// A player in the fixtures below.
    const UPSTAIRS: PlayerId = PlayerId::from_raw(11);

    /// A player in the fixtures below.
    const NOWHERE: PlayerId = PlayerId::from_raw(12);

    /// Build a table from a list of players and their patches.
    fn table_of(players: &[(PlayerId, PlayerPatch)]) -> PlayerTable {
        let mut table = PlayerTable::new();
        for (player, patch) in players {
            table.apply(*player, patch.clone());
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

    /// Listener list, sorted for comparison.
    fn sorted(listeners: &[PlayerId]) -> Vec<PlayerId> {
        let mut listeners = listeners.to_vec();
        listeners.sort_unstable();
        listeners
    }

    #[test]
    fn broadcast_ignores_game_state_entirely() {
        let players = table_of(&[(ANYONE, present())]);
        assert!(matches!(BroadcastRouter.compute(&players), Routing::Unrestricted));
    }

    #[test]
    fn hearer_list_relays_along_the_game_s_own_list() {
        let players = table_of(&[
            (
                SPEAKER,
                PlayerPatch {
                    local_with: Some(vec![NEAR]),
                    ..present()
                },
            ),
            (NEAR, present()),
            (FAR, present()),
        ]);

        let table = explicit(HearerListRouter.compute(&players));

        assert_eq!(sorted(table.local_listeners(SPEAKER)), vec![NEAR]);
        assert!(table.local_listeners(FAR).is_empty());
    }

    #[test]
    fn a_muted_speaker_reaches_nobody() {
        let players = table_of(&[
            (
                SPEAKER,
                PlayerPatch {
                    mute: Some(true),
                    deaf: Some(false),
                    local_with: Some(vec![NEAR]),
                    ..Default::default()
                },
            ),
            (NEAR, present()),
        ]);

        let table = explicit(HearerListRouter.compute(&players));

        assert!(!table.can_speak(SPEAKER));
        assert!(table.local_listeners(SPEAKER).is_empty());
    }

    #[test]
    fn a_deaf_listener_is_dropped_from_the_list() {
        let players = table_of(&[
            (
                SPEAKER,
                PlayerPatch {
                    local_with: Some(vec![DEAFENED, HEARING]),
                    ..present()
                },
            ),
            (
                DEAFENED,
                PlayerPatch {
                    deaf: Some(true),
                    ..present()
                },
            ),
            (HEARING, present()),
        ]);

        let table = explicit(HearerListRouter.compute(&players));

        assert_eq!(sorted(table.local_listeners(SPEAKER)), vec![HEARING]);
    }

    #[test]
    fn a_speaker_is_never_their_own_listener() {
        let players = table_of(&[(
            SPEAKER,
            PlayerPatch {
                local_with: Some(vec![SPEAKER]),
                ..present()
            },
        )]);

        let table = explicit(HearerListRouter.compute(&players));

        assert!(table.local_listeners(SPEAKER).is_empty());
    }

    #[test]
    fn radio_listeners_are_keyed_by_frequency() {
        let players = table_of(&[
            (
                SPEAKER,
                PlayerPatch {
                    hot_freqs: Some(vec![Freq(1459)]),
                    ..present()
                },
            ),
            (
                ASSISTANT,
                PlayerPatch {
                    hear_freqs: Some(vec![Freq(1459)]),
                    ..present()
                },
            ),
            (
                SECURITY_OFFICER,
                PlayerPatch {
                    hear_freqs: Some(vec![Freq(1359)]),
                    ..present()
                },
            ),
        ]);

        let table = explicit(HearerListRouter.compute(&players));

        assert_eq!(sorted(table.radio_listeners(Freq(1459))), vec![ASSISTANT]);
        assert_eq!(sorted(table.radio_listeners(Freq(1359))), vec![SECURITY_OFFICER]);
        assert!(table.radio_listeners(Freq(1)).is_empty());
    }

    #[test]
    fn transmitting_needs_the_frequency_to_be_hot() {
        let players = table_of(&[(
            SPEAKER,
            PlayerPatch {
                hot_freqs: Some(vec![Freq(1459)]),
                ..present()
            },
        )]);

        let table = explicit(HearerListRouter.compute(&players));

        assert!(table.can_transmit_on(SPEAKER, Freq(1459)));
        // Listening to a channel does not grant the right to talk on it.
        assert!(!table.can_transmit_on(SPEAKER, Freq(1359)));
    }

    #[test]
    fn a_deaf_player_hears_no_radio() {
        let players = table_of(&[(
            DEAFENED,
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
                SPEAKER,
                PlayerPatch {
                    position: Some(Position { x: 0, y: 0, z: 1 }),
                    ..present()
                },
            ),
            // Diagonally 7 away: inside a Chebyshev radius, outside a Euclidean one.
            (
                CORNER,
                PlayerPatch {
                    position: Some(Position { x: 7, y: 7, z: 1 }),
                    ..present()
                },
            ),
            (
                OUTSIDE,
                PlayerPatch {
                    position: Some(Position { x: 8, y: 0, z: 1 }),
                    ..present()
                },
            ),
        ]);

        let table = explicit(router.compute(&players));

        assert_eq!(sorted(table.local_listeners(SPEAKER)), vec![CORNER]);
    }

    #[test]
    fn proximity_never_carries_between_z_levels() {
        let players = table_of(&[
            (
                SPEAKER,
                PlayerPatch {
                    position: Some(Position { x: 0, y: 0, z: 1 }),
                    ..present()
                },
            ),
            (
                UPSTAIRS,
                PlayerPatch {
                    position: Some(Position { x: 0, y: 0, z: 2 }),
                    ..present()
                },
            ),
        ]);

        let table = explicit(ProximityRouter::default().compute(&players));

        assert!(table.local_listeners(SPEAKER).is_empty());
    }

    #[test]
    fn proximity_ignores_players_with_no_known_position() {
        let players = table_of(&[
            (
                SPEAKER,
                PlayerPatch {
                    position: Some(Position { x: 0, y: 0, z: 1 }),
                    ..present()
                },
            ),
            (NOWHERE, present()),
        ]);

        let table = explicit(ProximityRouter::default().compute(&players));

        assert!(table.local_listeners(SPEAKER).is_empty());
    }
}
