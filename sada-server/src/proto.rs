//! Signaling protocol spoken with the browser.
//!
//! Two transports, one version. The WebSocket opens the session and ends it: the handshake, and the refusal or the
//! goodbye that closes it. The WebRTC data channel carries the session itself, and is where anything added later
//! belongs. It lands in the worker that already owns the peer's `Rtc`, with no task to hop through and no command
//! to define, which the WebSocket cannot say of anything it carries.

use sada_common::{AuthCode, Ckey, SessionId};
use serde::{Deserialize, Serialize};

/// Version of this protocol.
///
/// The client sends the version it was built against and the server refuses
/// anything it does not recognise, so a stale cached page fails loudly instead
/// of misbehaving.
pub const PROTOCOL_VERSION: u32 = 1;

/// A message sent by the browser.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ClientMessage {
    /// Opens the session. Must be the first message.
    Hello {
        /// Protocol version the client was built against.
        protocol: u32,
        /// Single-use code from the game, absent for an anonymous session.
        #[serde(default)]
        auth_code: Option<AuthCode>,
    },
    /// The initial SDP offer that establishes the connection.
    Offer {
        /// Session description.
        sdp: String,
    },
    /// Graceful disconnect.
    Bye,
}

/// A message sent by the server.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ServerMessage {
    /// Accepts the connection, before any WebRTC state exists.
    ///
    /// Sent as soon as the handshake is validated, so a rejected client learns about it before the browser asks the
    /// user for a microphone. It carries no session id because none has been assigned yet, that arrives with the
    /// answer, once the client's offer has been accepted.
    Welcome {
        /// Protocol version the server speaks.
        protocol: u32,
        /// Player this connection was bound to, absent when anonymous.
        ckey: Option<Ckey>,
    },
    /// Answer to the client's initial offer.
    Answer {
        /// Session description.
        sdp: String,
        /// Identifier assigned to this session.
        session: SessionId,
    },
    /// Who is currently audible to this client.
    Speaking {
        /// Sessions currently being heard.
        sessions: Vec<SessionId>,
    },
    /// The request could not be handled.
    Error {
        /// Machine-readable reason.
        code: ErrorCode,
        /// Human-readable detail.
        message: String,
    },
    /// The server is closing the session.
    Bye {
        /// Why the session ended.
        reason: String,
    },
}

/// A message sent by the browser through the WebRTC channel.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ClientChannelMessage {
    /// Answer to an offer the server sent.
    Answer {
        /// Session description.
        sdp: String,
    },
    /// The client muted or unmuted itself.
    ///
    /// The client already stops sending; this lets the server stop relaying immediately rather than at the end of
    /// the talkspurt.
    Mute {
        /// Whether the microphone is now muted.
        muted: bool,
    },
}

/// A message sent by the server through the WebRTC channel.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ServerChannelMessage {
    /// Offer asking the client to accept more incoming audio slots.
    Offer {
        /// Session description.
        sdp: String,
    },
}

/// Machine-readable failure reason.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    /// The client's protocol version is not supported.
    UnsupportedProtocol,
    /// The supplied auth code was unknown or already used.
    BadAuthCode,
    /// Anonymous sessions are not permitted by configuration.
    AuthRequired,
    /// The message was valid but not allowed in the current state.
    UnexpectedMessage,
    /// The session description could not be processed.
    BadSdpOffer,
    /// Something went wrong that the client cannot act on.
    Internal,
}

impl ServerMessage {
    /// Build an error message.
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            code,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use sada_common::SessionId;

    use super::*;

    #[test]
    fn hello_accepts_an_absent_auth_code() {
        let decoded: ClientMessage = serde_json::from_str(r#"{"type":"hello","protocol":1}"#).unwrap();

        assert_eq!(
            decoded,
            ClientMessage::Hello {
                protocol: 1,
                auth_code: None,
            }
        );
    }

    #[test]
    fn messages_are_tagged_by_type() {
        let json = serde_json::to_string(&ClientMessage::Offer { sdp: "v=0".to_owned() }).unwrap();

        assert_eq!(json, r#"{"type":"offer","sdp":"v=0"}"#);
    }

    #[test]
    fn field_names_reach_the_wire_in_camel_case() {
        let json = serde_json::to_string(&ClientMessage::Hello {
            protocol: PROTOCOL_VERSION,
            auth_code: Some("AB12CD".into()),
        })
        .unwrap();

        assert!(json.contains("\"authCode\":\"AB12CD\""), "unexpected encoding: {json}");
    }

    #[test]
    fn client_messages_round_trip() {
        for message in [
            ClientMessage::Hello {
                protocol: PROTOCOL_VERSION,
                auth_code: Some("AB12CD".into()),
            },
            ClientMessage::Bye,
        ] {
            let json = serde_json::to_string(&message).unwrap();
            assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), message);
        }
    }

    #[test]
    fn server_messages_round_trip() {
        for message in [
            ServerMessage::Welcome {
                protocol: PROTOCOL_VERSION,
                ckey: Some("sefa".into()),
            },
            ServerMessage::Answer {
                sdp: "v=0".to_owned(),
                session: SessionId::new(1, 1),
            },
            ServerMessage::Speaking {
                sessions: vec![SessionId::new(1, 1), SessionId::new(2, 1)],
            },
            ServerMessage::error(ErrorCode::BadAuthCode, "unknown code"),
            ServerMessage::Bye {
                reason: "shutting down".to_owned(),
            },
        ] {
            let json = serde_json::to_string(&message).unwrap();
            assert_eq!(serde_json::from_str::<ServerMessage>(&json).unwrap(), message);
        }
    }

    #[test]
    fn channel_messages_round_trip() {
        let client = ClientChannelMessage::Answer { sdp: "v=0".to_owned() };
        let json = serde_json::to_string(&client).unwrap();

        assert_eq!(json, r#"{"type":"answer","sdp":"v=0"}"#);
        assert_eq!(serde_json::from_str::<ClientChannelMessage>(&json).unwrap(), client);

        let mute = ClientChannelMessage::Mute { muted: true };
        let json = serde_json::to_string(&mute).unwrap();

        assert_eq!(json, r#"{"type":"mute","muted":true}"#);
        assert_eq!(serde_json::from_str::<ClientChannelMessage>(&json).unwrap(), mute);

        let server = ServerChannelMessage::Offer { sdp: "v=0".to_owned() };
        let json = serde_json::to_string(&server).unwrap();

        assert_eq!(json, r#"{"type":"offer","sdp":"v=0"}"#);
        assert_eq!(serde_json::from_str::<ServerChannelMessage>(&json).unwrap(), server);
    }
}
