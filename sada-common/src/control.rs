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

use crate::ids::{AuthCode, Ckey, PlayerId, SessionId};

/// Version of this protocol.
///
/// The game refuses to run against a server reporting a different version; there is no negotiation, because both sides
/// ship together.
pub const PROTOCOL_VERSION: u16 = 1;

/// A radio frequency, in the tenths-of-a-megahertz units the game uses.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(transparent)]
pub struct Freq(pub u16);

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
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[derive_const(Default)]
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
    pub local_with: Option<Vec<PlayerId>>,
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

#[cfg(feature = "byondapi")]
impl TryFrom<sada_byondapi::sys::CByondValue> for PlayerPatch {
    type Error = String;

    fn try_from(value: sada_byondapi::sys::CByondValue) -> Result<Self, Self::Error> {
        use sada_byondapi::{
            BYONDAPI,
            byond::{self, read_to_vec},
        };

        if !unsafe { BYONDAPI.ByondValue_IsList(&value) } {
            return Err("a patch is a list".to_owned());
        }

        let list = read_to_vec(|buf, len| unsafe { BYONDAPI.Byond_ReadListAssoc(&value, buf, len) }, 8);

        let mut patch = Self::default();
        let mut refused = None;

        for &[key, value] in list.as_chunks::<2>().0 {
            if let Err(err) = patch.set(&String::from(key), value) {
                refused.get_or_insert(err);
            }

            byond::value_decref(&key);
            byond::value_decref(&value);
        }

        match refused {
            Some(err) => Err(err),
            None => Ok(patch),
        }
    }
}

#[cfg(feature = "byondapi")]
impl PlayerPatch {
    /// Set one field from the DM value the game gave for it.
    fn set(&mut self, field: &str, value: sada_byondapi::sys::CByondValue) -> Result<(), String> {
        match field {
            "mute" => self.mute = Some(dm::boolean(field, value)?),
            "deaf" => self.deaf = Some(dm::boolean(field, value)?),
            "is_admin" => self.is_admin = Some(dm::boolean(field, value)?),
            "ghost_ears" => self.ghost_ears = Some(dm::boolean(field, value)?),
            "position" => self.position = Some(dm::position(field, value)?),
            "local_with" => self.local_with = Some(dm::list(field, value, dm::player)?),
            "hot_freqs" => self.hot_freqs = Some(dm::list(field, value, dm::freq)?),
            "hear_freqs" => self.hear_freqs = Some(dm::list(field, value, dm::freq)?),
            "known_languages" => self.known_languages = Some(dm::list(field, value, dm::string)?),
            "current_language" => self.current_language = Some(dm::string(field, value)?),
            _ => return Err(format!("unknown patch field: {field:?}")),
        }

        Ok(())
    }
}

/// Reading patch fields out of the values DM hands over.
#[cfg(feature = "byondapi")]
mod dm {
    use sada_byondapi::{BYONDAPI, byond, sys::CByondValue};

    use super::{Freq, PlayerId, Position};

    /// Largest value [`position`] can carry, one byte per coordinate.
    const MAX_PACKED_POSITION: u32 = 0xFF_FFFF;

    /// Read a number.
    pub fn number(field: &str, value: CByondValue) -> Result<f32, String> {
        if !unsafe { BYONDAPI.ByondValue_IsNum(&value) } {
            return Err(format!("{field:?} wants a number"));
        }

        Ok(f32::from(value))
    }

    /// Read a boolean.
    ///
    /// DM has no boolean type: `TRUE` is the number 1, so anything non-zero is true.
    pub fn boolean(field: &str, value: CByondValue) -> Result<bool, String> { Ok(number(field, value)? != 0.0) }

    /// Read a string.
    pub fn string(field: &str, value: CByondValue) -> Result<String, String> {
        if !unsafe { BYONDAPI.ByondValue_IsStr(&value) } {
            return Err(format!("{field:?} wants a string"));
        }

        Ok(String::from(value))
    }

    /// Read a player id, which DM carries as a decimal string because a number would lose its low bits.
    pub fn player(field: &str, value: CByondValue) -> Result<PlayerId, String> {
        let token = string(field, value)?;

        token
            .parse()
            .map(PlayerId::from_raw)
            .map_err(|_| format!("{field:?} holds {token:?}, which is not a player id"))
    }

    /// Read a radio frequency.
    pub fn freq(field: &str, value: CByondValue) -> Result<Freq, String> {
        let freq = number(field, value)?;

        if freq < 0.0 || freq > f32::from(u16::MAX) || freq.fract() != 0.0 {
            return Err(format!("{field:?} holds {freq}, which is not a frequency"));
        }

        Ok(Freq(freq as u16))
    }

    /// Read a position out of the single number DM packs it into.
    ///
    /// One byte per coordinate, which is the whole of the 24 bits DM's bitwise operators and its single-precision
    /// numbers can carry exactly.
    pub fn position(field: &str, value: CByondValue) -> Result<Position, String> {
        let packed = number(field, value)?;

        if packed < 0.0 || packed > MAX_PACKED_POSITION as f32 || packed.fract() != 0.0 {
            return Err(format!("{field:?} holds {packed}, which is not a packed position"));
        }

        let packed = packed as u32;

        Ok(Position {
            x: ((packed >> 16) & 0xFF) as i32,
            y: ((packed >> 8) & 0xFF) as i32,
            z: (packed & 0xFF) as i32,
        })
    }

    /// Read a flat list, one `item` per element.
    ///
    /// The read creates a reference per element, so each is dropped here once `item` has finished with it.
    pub fn list<T>(
        field: &str,
        value: CByondValue,
        item: impl Fn(&str, CByondValue) -> Result<T, String>,
    ) -> Result<Vec<T>, String> {
        if !unsafe { BYONDAPI.ByondValue_IsList(&value) } {
            return Err(format!("{field:?} wants a list"));
        }

        let elements = byond::read_to_vec(|buf, len| unsafe { BYONDAPI.Byond_ReadList(&value, buf, len) }, 8);

        let mut items = Vec::with_capacity(elements.len());
        let mut refused = None;

        for element in elements {
            match item(field, element) {
                Ok(item) => items.push(item),
                Err(err) => {
                    refused.get_or_insert(err);
                },
            }

            byond::value_decref(&element);
        }

        match refused {
            Some(err) => Err(err),
            None => Ok(items),
        }
    }
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
    /// session gets bound to a player.
    RegisterCode {
        /// Code shown to the player.
        code: AuthCode,
        /// Player the code belongs to.
        player: PlayerId,
        /// Name the browser that redeems the code is greeted with.
        ///
        /// The server shows it and does nothing else with it.
        ckey: Ckey,
    },
    /// Ask which session, if any, a player has authenticated.
    CheckAuth {
        /// Player to look up.
        player: PlayerId,
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
        player: PlayerId,
        /// Fields that changed.
        patch: PlayerPatch,
    },
    /// Forget a player entirely, e.g. on disconnect.
    RemovePlayer {
        /// Player to drop.
        player: PlayerId,
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
        protocol: u16,
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

#[cfg(feature = "byondapi")]
impl From<ControlResponse> for sada_byondapi::sys::CByondValue {
    fn from(value: ControlResponse) -> Self {
        use sada_byondapi::{byond, sys::CByondValue};

        match value {
            ControlResponse::Ok => byond::new(c"/datum/sada_response/ok", &[]),
            ControlResponse::Version { protocol, version } => {
                let version = CByondValue::from(version);

                let value = byond::new(c"/datum/sada_response/version", &[CByondValue::from(protocol), version]);

                byond::value_decref(&version);

                value
            },
            ControlResponse::Session { session } => {
                let session = session
                    .map(|s| CByondValue::from(s.token()))
                    .unwrap_or(CByondValue::NULL);

                let value = byond::new(c"/datum/sada_response/session", &[session]);

                byond::value_decref(&session);

                value
            },
            ControlResponse::Events(events) => {
                let events = events.into_iter().map(Into::into).collect::<Vec<_>>();

                let value = byond::new(c"/datum/sada_response/events", &events);

                events.into_iter().for_each(|e| byond::value_decref(&e));

                value
            },
            ControlResponse::Batch(responses) => {
                let responses = responses.into_iter().map(Into::into).collect::<Vec<_>>();

                let value = byond::new(c"/datum/sada_response/batch", &responses);

                responses.into_iter().for_each(|r| byond::value_decref(&r));

                value
            },
            ControlResponse::Error { message } => {
                let message = CByondValue::from(message);

                let value = byond::new(c"/datum/sada_response/error", &[message]);

                byond::value_decref(&message);

                value
            },
        }
    }
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
        player: PlayerId,
        /// Session now bound to them.
        session: SessionId,
    },
    /// A bound session went away.
    Disconnected {
        /// Player whose session ended.
        player: PlayerId,
        /// Session that ended.
        session: SessionId,
    },
    /// Who is currently being heard speaking, and by whom.
    ///
    /// This is full state rather than an edge, so the game can drive speech
    /// bubbles by set difference without tracking transitions itself.
    Speaking {
        /// Player being heard.
        speaker: PlayerId,
        /// Players currently hearing them.
        listeners: Vec<PlayerId>,
    },
    /// A player heard speech and the game should render a chat line for it.
    Heard {
        /// Player who spoke.
        speaker: PlayerId,
        /// Player who heard them.
        listener: PlayerId,
        /// Frequency it arrived on, or `None` for local speech.
        channel: Option<Freq>,
        /// Language it was spoken in.
        language: Option<String>,
    },
}

#[cfg(feature = "byondapi")]
impl From<ControlEvent> for sada_byondapi::sys::CByondValue {
    fn from(value: ControlEvent) -> Self {
        use std::iter;

        use sada_byondapi::{byond, sys::CByondValue};

        match value {
            ControlEvent::Authenticated { player, session } => {
                let player = CByondValue::from(player.to_string());
                let session = CByondValue::from(session.token());

                let value = byond::new(c"/datum/sada_event/authenticated", &[player, session]);

                byond::value_decref(&player);
                byond::value_decref(&session);

                value
            },
            ControlEvent::Disconnected { player, session } => {
                let player = CByondValue::from(player.to_string());
                let session = CByondValue::from(session.token());

                let value = byond::new(c"/datum/sada_event/disconnected", &[player, session]);

                byond::value_decref(&player);
                byond::value_decref(&session);

                value
            },
            ControlEvent::Speaking { speaker, listeners } => {
                let args = Vec::from_iter(
                    iter::once(CByondValue::from(speaker.to_string()))
                        .chain(listeners.into_iter().map(|p| CByondValue::from(p.to_string()))),
                );

                let value = byond::new(c"/datum/sada_event/speaking", &args);

                args.into_iter().for_each(|a| byond::value_decref(&a));

                value
            },
            ControlEvent::Heard {
                speaker,
                listener,
                channel,
                language,
            } => {
                let speaker = CByondValue::from(speaker.to_string());
                let listener = CByondValue::from(listener.to_string());
                let channel = channel.map(|f| CByondValue::from(f.0)).unwrap_or(CByondValue::NULL);
                let language = language.map(CByondValue::from).unwrap_or(CByondValue::NULL);

                let value = byond::new(c"/datum/sada_event/heard", &[speaker, listener, channel, language]);

                byond::value_decref(&speaker);
                byond::value_decref(&listener);
                byond::value_decref(&language);

                value
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ControlEvent, ControlRequest, ControlResponse, Freq, PlayerPatch, Position, Transmit};
    use crate::ids::{PlayerId, SessionId};

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
            player: PlayerId::from_raw(1),
            patch: PlayerPatch {
                mute: Some(false),
                deaf: Some(false),
                is_admin: Some(true),
                ghost_ears: None,
                position: Some(Position { x: 10, y: 20, z: 2 }),
                local_with: Some(vec![PlayerId::from_raw(2)]),
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
            ControlRequest::RemovePlayer {
                player: PlayerId::from_raw(3),
            },
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
                player: PlayerId::from_raw(1),
                session: SessionId::new(1, 1),
            },
            ControlEvent::Speaking {
                speaker: PlayerId::from_raw(1),
                listeners: vec![PlayerId::from_raw(2), PlayerId::from_raw(3)],
            },
            ControlEvent::Heard {
                speaker: PlayerId::from_raw(1),
                listener: PlayerId::from_raw(2),
                channel: Some(Freq(1459)),
                language: Some("/datum/language/common".to_owned()),
            },
        ] {
            assert_eq!(round_trip(&event), event);
        }
    }
}
