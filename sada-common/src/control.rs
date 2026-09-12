//! Control protocol spoken between the BYOND game server and the voice server.
//!
//! The game drives this channel: it opens the socket, sends a request and reads exactly one response. That shape is
//! forced by BYOND, whose `call_ext` blocks the game thread, so the server can never push unsolicited frames. Anything
//! the server needs to tell the game is queued and collected with [`ControlRequest::PollEvents`].
//!
//! Two consequences shaped the design:
//!
//! * The game recomputes voice state for many players per tick, so [`ControlRequest::Batch`] exists to keep that one
//!   round trip rather than one per player.
//! * State is sent as deltas ([`PlayerPatch`]), because the game already tracks which fields changed and most ticks
//!   change very little.

use serde::{Deserialize, Serialize};

use crate::ids::{AuthCode, Ckey, SessionId};

/// Version of this protocol.
///
/// The game refuses to run against a server reporting a different version; there is no negotiation, because both sides
/// ship together.
pub const PROTOCOL_VERSION: u32 = 1;

/// A radio frequency, in the tenths-of-a-megahertz units the game uses.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(transparent)]
pub struct Freq(pub u32);

/// A position in the game world.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct Position {
    /// East-west tile coordinate.
    pub x: i32,
    /// North-south tile coordinate.
    pub y: i32,
    /// Z-level; sound never carries between them.
    pub z: i32,
}

/// What a session is transmitting on.
///
/// Absence of this, as `Option::None`, is the third state: not transmitting at all. Keeping all three in one value is
/// what lets [`ControlRequest::SetTransmit`] be a single request rather than a set and a clear.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Transmit {
    /// Local speech, heard by whoever the game says is nearby.
    Local,
    /// Radio speech on a frequency.
    Radio(Freq),
}

/// Delta update for one player's voice-relevant state.
///
/// Every field is optional and absent means "unchanged". The server keeps the accumulated state; the game only ever
/// sends what moved.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PlayerPatch {
    /// Whether the player is currently unable to speak at all.
    ///
    /// This is the game's notion of muteness: unconscious, dead, gagged; and
    /// changes at game tick cadence. It is not the browser's mute button, which
    /// the client applies to itself immediately.
    pub mute: Option<bool>,
    /// Whether the player is currently unable to hear.
    pub deaf: Option<bool>,
    /// Whether the player holds admin rights.
    pub is_admin: Option<bool>,
    /// Whether a dead player has been granted omnipresent hearing.
    pub ghost_ears: Option<bool>,
    /// Where the player is standing.
    pub position: Option<Position>,
    /// Players who can hear this one speak locally.
    ///
    /// The game computes this because only it knows about walls, doors and
    /// holopads. Used by the hearer-list routing policy.
    pub local_with: Option<Vec<Ckey>>,
    /// Frequencies this player can currently transmit on.
    pub hot_freqs: Option<Vec<Freq>>,
    /// Frequencies this player can currently receive.
    pub hear_freqs: Option<Vec<Freq>>,
    /// Languages this player understands.
    pub known_languages: Option<Vec<String>>,
    /// Language this player is currently speaking.
    pub current_language: Option<String>,
}

impl PlayerPatch {
    /// Whether the patch carries no changes at all.
    #[must_use]
    pub fn is_empty(&self) -> bool { *self == Self::default() }
}

/// A request sent from the BYOND bridge to the voice server.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlRequest {
    /// Ask the server for its protocol version.
    Version,
    /// Register a single-use code the game has shown to a player.
    ///
    /// The player types the code into the web client, which is how a browser
    /// session gets bound to a ckey.
    RegisterCode {
        /// Code shown to the player.
        code: AuthCode,
        /// Player the code belongs to.
        ckey: Ckey,
    },
    /// Ask which session, if any, a player has authenticated.
    CheckAuth {
        /// Player to look up.
        ckey: Ckey,
    },
    /// Update what a session is transmitting on, if anything.
    ///
    /// `None` stops them transmitting; [`Transmit::Local`] is local speech and
    /// [`Transmit::Radio`] names a frequency. This is applied immediately
    /// rather than folded into the next state snapshot, because a late release
    /// leaves the microphone hot.
    SetTransmit {
        /// Session to update.
        session: SessionId,
        /// What they are transmitting on, or `None` to stop them.
        transmit: Option<Transmit>,
    },
    /// Apply a delta to a player's state.
    PatchPlayer {
        /// Player to update.
        ckey: Ckey,
        /// Fields that changed.
        patch: PlayerPatch,
    },
    /// Forget a player entirely, e.g. on disconnect.
    RemovePlayer {
        /// Player to drop.
        ckey: Ckey,
    },
    /// Apply several requests in one round trip.
    ///
    /// Nested batches are rejected; the server answers with
    /// [`ControlResponse::Batch`] holding one response per element, in order.
    Batch(Vec<ControlRequest>),
    /// Collect queued server-to-game events.
    PollEvents {
        /// Maximum number of events to return.
        max: u16,
    },
}

/// A response returned by the voice server's control socket.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlResponse {
    /// The request was applied and carries no result.
    Ok,
    /// Server protocol version.
    Version {
        /// Value of [`PROTOCOL_VERSION`] as built into the server.
        protocol: u32,
        /// Human-readable server build version.
        version: String,
    },
    /// The session bound to a player, if any.
    Session {
        /// Bound session, or `None` when the player has not authenticated.
        session: Option<SessionId>,
    },
    /// Events queued since the last poll.
    Events(Vec<ControlEvent>),
    /// One response per element of a [`ControlRequest::Batch`].
    Batch(Vec<ControlResponse>),
    /// The request could not be applied.
    Error {
        /// Human-readable error message.
        message: String,
    },
}

/// Something the server needs to tell the game about.
///
/// Queued server-side and drained by [`ControlRequest::PollEvents`], because the control socket cannot carry
/// unsolicited frames.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlEvent {
    /// A browser redeemed a code and is now bound to a player.
    Authenticated {
        /// Player that authenticated.
        ckey: Ckey,
        /// Session now bound to them.
        session: SessionId,
    },
    /// A bound session went away.
    Disconnected {
        /// Player whose session ended.
        ckey: Ckey,
        /// Session that ended.
        session: SessionId,
    },
    /// Who is currently being heard speaking, and by whom.
    ///
    /// This is full state rather than an edge, so the game can drive speech
    /// bubbles by set difference without tracking transitions itself.
    Speaking {
        /// Player being heard.
        speaker: Ckey,
        /// Players currently hearing them.
        listeners: Vec<Ckey>,
    },
    /// A player heard speech and the game should render a chat line for it.
    Heard {
        /// Player who spoke.
        speaker: Ckey,
        /// Player who heard them.
        listener: Ckey,
        /// Frequency it arrived on, or `None` for local speech.
        channel: Option<Freq>,
        /// Language it was spoken in.
        language: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::{ControlEvent, ControlRequest, ControlResponse, Freq, PlayerPatch, Position, Transmit};
    use crate::ids::SessionId;

    /// Encode and decode a value through the wire format.
    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + for<'de> serde::Deserialize<'de>,
    {
        let bytes = postcard::to_stdvec(value).unwrap();
        postcard::from_bytes(&bytes).unwrap()
    }

    #[test]
    fn an_empty_patch_is_recognized() {
        assert!(PlayerPatch::default().is_empty());
        assert!(
            !PlayerPatch {
                deaf: Some(true),
                ..Default::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn a_full_patch_round_trips() {
        let request = ControlRequest::PatchPlayer {
            ckey: "sefa".into(),
            patch: PlayerPatch {
                mute: Some(false),
                deaf: Some(false),
                is_admin: Some(true),
                ghost_ears: None,
                position: Some(Position { x: 10, y: 20, z: 2 }),
                local_with: Some(vec!["other".into()]),
                hot_freqs: Some(vec![Freq(1459)]),
                hear_freqs: Some(vec![Freq(1459), Freq(1351)]),
                known_languages: Some(vec!["/datum/language/common".to_owned()]),
                current_language: Some("/datum/language/common".to_owned()),
            },
        };

        assert_eq!(round_trip(&request), request);
    }

    #[test]
    fn a_batch_round_trips_in_order() {
        let request = ControlRequest::Batch(vec![
            ControlRequest::Version,
            ControlRequest::RemovePlayer { ckey: "gone".into() },
        ]);

        assert_eq!(round_trip(&request), request);
    }

    #[test]
    fn transmitting_distinguishes_silence_from_local_from_radio() {
        let session = SessionId::new(1, 1);

        let silent = ControlRequest::SetTransmit {
            session,
            transmit: None,
        };
        let local = ControlRequest::SetTransmit {
            session,
            transmit: Some(Transmit::Local),
        };
        let radio = ControlRequest::SetTransmit {
            session,
            transmit: Some(Transmit::Radio(Freq(1459))),
        };

        assert_ne!(silent, local);
        assert_ne!(local, radio);

        assert_eq!(round_trip(&silent), silent);
        assert_eq!(round_trip(&local), local);
        assert_eq!(round_trip(&radio), radio);
    }

    #[test]
    fn responses_round_trip() {
        for response in [
            ControlResponse::Ok,
            ControlResponse::Session {
                session: Some(SessionId::new(3, 1)),
            },
            ControlResponse::Session { session: None },
            ControlResponse::Batch(vec![ControlResponse::Ok, ControlResponse::Ok]),
            ControlResponse::Error {
                message: "nope".to_owned(),
            },
        ] {
            assert_eq!(round_trip(&response), response);
        }
    }

    #[test]
    fn events_round_trip() {
        for event in [
            ControlEvent::Authenticated {
                ckey: "sefa".into(),
                session: SessionId::new(1, 1),
            },
            ControlEvent::Speaking {
                speaker: "sefa".into(),
                listeners: vec!["a".into(), "b".into()],
            },
            ControlEvent::Heard {
                speaker: "sefa".into(),
                listener: "a".into(),
                channel: Some(Freq(1459)),
                language: Some("/datum/language/common".to_owned()),
            },
        ] {
            assert_eq!(round_trip(&event), event);
        }
    }
}
