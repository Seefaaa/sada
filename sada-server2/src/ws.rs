//! One browser connection.
//!
//! This task does framing and the handshake, and nothing else: it owns no WebRTC state and makes no routing decisions.
//! Everything it learns is turned into a command for the worker or the directory.

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt as _, Stream, StreamExt as _, stream::SplitSink};
use sada_common::{Ckey, SessionId};
use str0m::change::SdpOffer;
use tokio::sync::mpsc;

use crate::{
    directory::DirectoryCommand,
    http::AppState,
    proto::{ClientMessage, ErrorCode, PROTOCOL_VERSION, ServerMessage},
    sfu::{ConnectError, WorkerCommand},
};

/// How many server messages may be queued for one browser.
const SIGNAL_BUFFER: usize = 16;

/// Serve one browser connection until it closes.
pub async fn serve(socket: WebSocket, state: AppState) {
    let (mut sink, mut stream) = socket.split();

    let Some(ckey) = handshake(&mut sink, &mut stream, &state).await else {
        return;
    };

    let welcome = ServerMessage::Welcome {
        protocol: PROTOCOL_VERSION,
        ckey: ckey.clone(),
    };
    if send(&mut sink, &welcome).await.is_err() {
        return;
    }

    let (signal_tx, mut signal_rx) = mpsc::channel(SIGNAL_BUFFER);

    let Some(session) = establish(&mut sink, &mut stream, &state, ckey.clone(), signal_tx).await else {
        return;
    };

    if let Some(ckey) = ckey {
        state.directory.send(DirectoryCommand::Bind { ckey, session }).await;
    }

    info!(%session, "session established");

    loop {
        tokio::select! {
            incoming = stream.next() => {
                let Some(Ok(message)) = incoming else { break };
                if !on_client_message(&state, session, message).await {
                    break;
                }
            },
            outgoing = signal_rx.recv() => {
                let Some(message) = outgoing else { break };
                if send(&mut sink, &message).await.is_err() {
                    break;
                }
            },
        }
    }

    info!(%session, "session closed");

    state.worker.send(WorkerCommand::Disconnect { session }).await;
}

/// Read the opening `Hello` and decide who the client is.
///
/// Returns the bound player, or `None` if the connection was rejected, in which case the reason has already been sent.
async fn handshake(
    sink: &mut SplitSink<WebSocket, Message>,
    stream: &mut (impl Stream<Item = Result<Message, axum::Error>> + Unpin),
    state: &AppState,
) -> Option<Option<Ckey>> {
    let hello = read_message(stream).await?;

    let ClientMessage::Hello { protocol, auth_code } = hello else {
        reject(sink, ErrorCode::UnexpectedMessage, "expected hello first").await;
        return None;
    };

    if protocol != PROTOCOL_VERSION {
        reject(
            sink,
            ErrorCode::UnsupportedProtocol,
            format!("server speaks protocol {PROTOCOL_VERSION}"),
        )
        .await;
        return None;
    }

    let ckey = match auth_code {
        Some(code) => match state.directory.redeem(code).await {
            Some(ckey) => Some(ckey),
            None => {
                reject(sink, ErrorCode::BadAuthCode, "code is not valid").await;
                return None;
            },
        },
        None if state.config.auth.allow_anonymous => None,
        None => {
            reject(sink, ErrorCode::AuthRequired, "an auth code is required").await;
            return None;
        },
    };

    Some(ckey)
}

/// Read the client's offer and answer it, returning the new session.
async fn establish(
    sink: &mut SplitSink<WebSocket, Message>,
    stream: &mut (impl Stream<Item = Result<Message, axum::Error>> + Unpin),
    state: &AppState,
    ckey: Option<Ckey>,
    signal: mpsc::Sender<ServerMessage>,
) -> Option<SessionId> {
    let message = read_message(stream).await?;

    let ClientMessage::Offer { sdp } = message else {
        reject(sink, ErrorCode::UnexpectedMessage, "expected an offer").await;
        return None;
    };

    let Ok(offer) = SdpOffer::from_sdp_string(&sdp) else {
        reject(sink, ErrorCode::BadSdpOffer, "the offer was not valid SDP").await;
        return None;
    };

    let accepted = match state.worker.connect(offer, ckey, signal).await {
        Ok(accepted) => accepted,
        Err(err) => {
            warn!(?err, "could not start a session");
            match err {
                ConnectError::Offer => reject(sink, ErrorCode::BadSdpOffer, "the offer was not accepted").await,
                ConnectError::Unavailable => reject(sink, ErrorCode::Internal, "the media worker is not running").await,
                ConnectError::Host => reject(sink, ErrorCode::Internal, "host candidate could not be built").await,
            }
            return None;
        },
    };

    let answer = ServerMessage::Answer {
        sdp: accepted.answer,
        session: accepted.session,
    };
    send(sink, &answer).await.ok()?;

    Some(accepted.session)
}

/// Act on one message from an established client. Returns whether to continue.
async fn on_client_message(state: &AppState, session: SessionId, message: Message) -> bool {
    let text = match message {
        Message::Text(text) => text,
        Message::Close(_) => return false,
        _ => return true,
    };

    let Ok(message) = serde_json::from_str::<ClientMessage>(&text) else {
        warn!(%session, "ignoring an unparseable message");
        return true;
    };

    match message {
        ClientMessage::Answer { sdp } => {
            state.worker.send(WorkerCommand::Answer { session, sdp }).await;
        },
        ClientMessage::Mute { muted } => {
            state.worker.send(WorkerCommand::Mute { session, muted }).await;
        },
        ClientMessage::Bye => return false,
        ClientMessage::Offer { .. } | ClientMessage::Hello { .. } => {
            warn!(%session, "client sent a message that is not valid mid-session");
        },
    }

    true
}

/// Read and decode the next client message, or `None` if the socket ended.
async fn read_message(
    stream: &mut (impl Stream<Item = Result<Message, axum::Error>> + Unpin),
) -> Option<ClientMessage> {
    loop {
        match stream.next().await? {
            Ok(Message::Text(text)) => return serde_json::from_str(&text).ok(),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {},
        }
    }
}

/// Send one message to the client.
async fn send(sink: &mut SplitSink<WebSocket, Message>, message: &ServerMessage) -> Result<(), axum::Error> {
    let json = match serde_json::to_string(message) {
        Ok(json) => json,
        Err(err) => {
            error!(?err, "failed to encode a server message");
            return Ok(());
        },
    };

    sink.send(Message::Text(json.into())).await
}

/// Tell the client why it was turned away, then let the socket close.
async fn reject(sink: &mut SplitSink<WebSocket, Message>, code: ErrorCode, message: impl Into<String>) {
    let message = message.into();
    info!(?code, %message, "rejecting a connection");
    let _ = send(sink, &ServerMessage::error(code, message)).await;
}
