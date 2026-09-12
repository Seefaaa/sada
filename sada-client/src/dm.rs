//! The shapes the game sees.
//!
//! BYOND cannot hold everything the wire types carry, so responses are translated here rather than serialized
//! straight: DM numbers are single-precision floats, which a 64-bit session id does not survive.

use sada_common::{Ckey, ControlEvent, ControlResponse, Freq, PlayerPatch, Position, SessionId};
use serde::{Deserialize, Serialize};

/// Encodes a [`ControlResponse`] into a JSON string. If encoding fails, returns an error message as JSON string.
pub fn encode_response(response: ControlResponse) -> String {
    serde_json::to_string(&DmResponse::from(response)).unwrap_or_else(|err| {
        serde_json::json!({
            "error": {
                "message": format!("failed to encode response: {err}"),
            },
        })
        .to_string()
    })
}

/// A [`ControlResponse`] in the shape BYOND can hold on to.
///
/// The only difference from the wire type is that session ids travel as decimal strings. They are `u64` and DM numbers
/// are single-precision floats, so anything past 2^24 comes back mangled; the very first session id is already above
/// 2^32 because it packs a generation into the high half. The game treats the string as an opaque token and hands it
/// straight back to `set_ptt`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DmResponse {
    /// The request was applied and carries no result.
    Ok,
    /// Server protocol version.
    Version {
        /// Protocol the server speaks.
        protocol: u32,
        /// Human-readable server build version.
        version: String,
    },
    /// The session bound to a player, if any.
    Session {
        /// Bound session, or `null` when the player has not authenticated.
        session: Option<String>,
    },
    /// Events queued since the last poll.
    Events(Vec<DmEvent>),
    /// One response per element of a batch.
    Batch(Vec<DmResponse>),
    /// The request could not be applied.
    Error {
        /// Human-readable error message.
        message: String,
    },
}

impl From<ControlResponse> for DmResponse {
    fn from(response: ControlResponse) -> Self {
        match response {
            ControlResponse::Ok => Self::Ok,
            ControlResponse::Version { protocol, version } => Self::Version { protocol, version },
            ControlResponse::Session { session } => Self::Session {
                session: session.map(token),
            },
            ControlResponse::Events(events) => Self::Events(events.into_iter().map(DmEvent::from).collect()),
            ControlResponse::Batch(responses) => Self::Batch(responses.into_iter().map(Self::from).collect()),
            ControlResponse::Error { message } => Self::Error { message },
        }
    }
}

/// A [`ControlEvent`] in the shape BYOND can hold on to.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(serde::Deserialize, PartialEq))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DmEvent {
    /// A browser redeemed a code and is now bound to a player.
    Authenticated {
        /// Player that authenticated.
        ckey: Ckey,
        /// Session now bound to them.
        session: String,
    },
    /// A bound session went away.
    Disconnected {
        /// Player whose session ended.
        ckey: Ckey,
        /// Session that ended.
        session: String,
    },
    /// Who is currently being heard speaking, and by whom.
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
        /// Frequency it arrived on, or `null` for local speech.
        channel: Option<Freq>,
        /// Language it was spoken in.
        language: Option<String>,
    },
}

impl From<ControlEvent> for DmEvent {
    fn from(event: ControlEvent) -> Self {
        match event {
            ControlEvent::Authenticated { ckey, session } => Self::Authenticated {
                ckey,
                session: token(session),
            },
            ControlEvent::Disconnected { ckey, session } => Self::Disconnected {
                ckey,
                session: token(session),
            },
            ControlEvent::Speaking { speaker, listeners } => Self::Speaking { speaker, listeners },
            ControlEvent::Heard {
                speaker,
                listener,
                channel,
                language,
            } => Self::Heard {
                speaker,
                listener,
                channel,
                language,
            },
        }
    }
}

/// One boolean of a patch as BYOND writes it.
///
/// DM has no boolean type and `json_encode` renders `TRUE` as `1`, so a plain `bool` field would refuse every patch
/// the game sends.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(untagged)]
pub enum DmBool {
    /// A real JSON boolean, which is what every other caller sends.
    Bool(bool),
    /// BYOND's rendering of `TRUE` and `FALSE`.
    Number(f32),
}

impl From<DmBool> for bool {
    fn from(value: DmBool) -> Self {
        match value {
            DmBool::Bool(value) => value,
            DmBool::Number(value) => value != 0.0,
        }
    }
}

/// A [`PlayerPatch`] in the shape the game sends it.
///
/// Absent fields mean unchanged, exactly as in the wire type; the game only fills in what actually moved.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PatchJson {
    /// Whether the game forbids this player from speaking.
    mute: Option<DmBool>,
    /// Whether the game forbids this player from hearing.
    deaf: Option<DmBool>,
    /// Whether the player holds admin rights.
    is_admin: Option<DmBool>,
    /// Whether a dead player has been granted omnipresent hearing.
    ghost_ears: Option<DmBool>,
    /// Where the player is standing.
    position: Option<Position>,
    /// Players who can hear this one speak locally.
    local_with: Option<Vec<Ckey>>,
    /// Frequencies this player may transmit on.
    hot_freqs: Option<Vec<Freq>>,
    /// Frequencies this player receives.
    hear_freqs: Option<Vec<Freq>>,
    /// Languages this player understands.
    known_languages: Option<Vec<String>>,
    /// Language this player is currently speaking.
    current_language: Option<String>,
}

impl From<PatchJson> for PlayerPatch {
    fn from(patch: PatchJson) -> Self {
        Self {
            mute: patch.mute.map(bool::from),
            deaf: patch.deaf.map(bool::from),
            is_admin: patch.is_admin.map(bool::from),
            ghost_ears: patch.ghost_ears.map(bool::from),
            position: patch.position,
            local_with: patch.local_with,
            hot_freqs: patch.hot_freqs,
            hear_freqs: patch.hear_freqs,
            known_languages: patch.known_languages,
            current_language: patch.current_language,
        }
    }
}

/// Render a session id as the opaque token the game passes back.
pub fn token(session: SessionId) -> String { session.as_raw().to_string() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_events_are_tagged() {
        let from_dm = DmEvent::Authenticated {
            ckey: "sefa".into(),
            session: "123".into(),
        };
        let from_json = serde_json::from_str(r#"{"type":"authenticated","ckey":"sefa","session":"123"}"#).unwrap();
        assert_eq!(from_dm, from_json);
    }

    #[test]
    fn byond_booleans_are_understood() {
        // DM has no boolean type: json_encode writes TRUE as 1. Anything else sending real booleans still works.
        let from_dm: PatchJson = serde_json::from_str(r#"{"mute":1,"deaf":0}"#).expect("byond shape");
        let from_json: PatchJson = serde_json::from_str(r#"{"mute":true,"deaf":false}"#).expect("json shape");

        assert_eq!(PlayerPatch::from(from_dm).mute, Some(true));
        assert_eq!(PlayerPatch::from(from_json).deaf, Some(false));
        assert!(bool::from(DmBool::Number(-1.0)), "any nonzero number is truthy");
    }
}
