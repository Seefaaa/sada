//! Control channel for the BYOND game bridge.
//!
//! The game asks and this answers, one response per request and in order, which is the whole of the correlation either
//! side needs. Events the server produces travel the same channel without being asked for, batched so a busy room does
//! not turn into a frame per listener.

use std::{sync::Arc, time::Duration};

use sada_common::{ControlEvent, ControlMessage, ControlRequest, ControlResponse, PROTOCOL_VERSION};
use sada_ipc::{Endpoint, Listener, Receiver, Sender};
use sada_utils::shutdown::Shutdown;
use thiserror::Error;
use tokio::{
    sync::{Semaphore, broadcast},
    time::sleep,
};

use crate::directory::{DirectoryCommand, DirectoryHandle};

/// Maximum number of control clients served at once.
///
/// The game opens one connection; the rest of the budget is for a debugging tool, a reconnect racing a stale connection
/// or whatever. Once they are all taken the listener stops accepting.
const MAX_CLIENTS: usize = 4;

/// Largest number of events put in one frame.
const MAX_EVENTS_PER_FRAME: usize = 256;

/// How long the listener waits before accepting again after a failure.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Run the control listener until shutdown.
pub async fn serve(endpoint: Endpoint, directory: DirectoryHandle, mut shutdown: Shutdown) -> Result<(), Error> {
    let mut listener = Listener::bind(&endpoint)?;
    info!(%endpoint, "control channel listening");

    let semaphore = Arc::new(Semaphore::new(MAX_CLIENTS));

    loop {
        let permit = tokio::select! {
            () = shutdown.recv() => break,
            permit = Arc::clone(&semaphore).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => break,
            },
        };

        let accepted = tokio::select! {
            () = shutdown.recv() => break,
            accepted = listener.accept() => accepted,
        };

        let (to_game, from_game) = match accepted {
            Ok(channel) => channel,
            Err(err) => {
                warn!(?err, "failed to accept a control client");

                tokio::select! {
                    () = shutdown.recv() => break,
                    () = sleep(ACCEPT_RETRY_DELAY) => continue,
                }
            },
        };

        let directory = directory.clone();

        tokio::spawn(async move {
            let _permit = permit;
            if let Err(err) = serve_client(to_game, from_game, directory).await {
                warn!(?err, "control connection ended with an error");
            }
        });
    }

    info!("control channel stopped");

    Ok(())
}

/// Serve one control client until it goes away.
async fn serve_client(
    to_game: Sender<ControlMessage>,
    from_game: Receiver<ControlRequest>,
    directory: DirectoryHandle,
) -> Result<(), Error> {
    // Subscribing before asking for the snapshot makes an event that lands in between a repeat rather than a gap, and a
    // repeated Authenticated tells the game what it already knew.
    let mut events = directory.subscribe();

    catch_up(&to_game, &directory).await?;

    loop {
        tokio::select! {
            request = from_game.recv_async() => {
                let Ok(request) = request else { break };

                let response = handle(request, &directory, true).await;

                to_game.send_async(ControlMessage::Response(response)).await?;
            },
            event = events.recv() => {
                // This task holds a handle to the directory, so the events outlive it: a closed stream means the
                // server is coming down, and selecting on it again would spin.
                if matches!(&event, Err(broadcast::error::RecvError::Closed)) {
                    break;
                }

                let batch = collect(event, &mut events);

                if batch.is_empty() {
                    continue;
                }

                to_game.send_async(ControlMessage::Events(batch)).await?;
            },
        }
    }

    Ok(())
}

/// Tell a fresh connection which sessions are already bound.
///
/// Events are only sent as they happen, so a game that connects to a server that has been running without it, or that
/// reconnected after losing the channel, would otherwise never hear about the sessions it missed. The full set goes
/// first and always, empty included, because it is the only thing that tells the game which of the sessions it
/// remembers the server no longer has.
async fn catch_up(to_game: &Sender<ControlMessage>, directory: &DirectoryHandle) -> Result<(), Error> {
    let bound = directory.bindings().await;

    let synchronized = ControlEvent::Synchronized {
        players: bound.iter().map(|&(player, _)| player).collect(),
    };

    debug!(bound = bound.len(), "telling a fresh control client what is bound");

    to_game.send_async(ControlMessage::Events(vec![synchronized])).await?;

    for batch in bound.chunks(MAX_EVENTS_PER_FRAME) {
        let events = batch
            .iter()
            .map(|&(player, session)| ControlEvent::Authenticated { player, session })
            .collect();

        to_game.send_async(ControlMessage::Events(events)).await?;
    }

    Ok(())
}

/// Take the event that just arrived and whatever else is already waiting, up to one frame's worth.
fn collect(
    first: Result<ControlEvent, broadcast::error::RecvError>,
    events: &mut broadcast::Receiver<ControlEvent>,
) -> Vec<ControlEvent> {
    let mut batch = Vec::new();

    match first {
        Ok(event) => batch.push(event),
        // The oldest events are gone and there is nothing to be done about it, but the game should hear that they were.
        Err(broadcast::error::RecvError::Lagged(missed)) => warn!(missed, "control connection fell behind the events"),
        Err(broadcast::error::RecvError::Closed) => return batch,
    }

    while batch.len() < MAX_EVENTS_PER_FRAME {
        match events.try_recv() {
            Ok(event) => batch.push(event),
            Err(broadcast::error::TryRecvError::Lagged(missed)) => {
                warn!(missed, "control connection fell behind the events");
            },
            Err(_) => break,
        }
    }

    batch
}

/// Apply one request.
///
/// `top_level` is false inside a batch, which is how nesting is refused.
async fn handle(request: ControlRequest, directory: &DirectoryHandle, top_level: bool) -> ControlResponse {
    match request {
        ControlRequest::Version => ControlResponse::Version {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },

        ControlRequest::RegisterCode { code, player, ckey } => {
            directory
                .send(DirectoryCommand::RegisterCode { code, player, ckey })
                .await;
            ControlResponse::Ok
        },

        ControlRequest::CheckAuth { player } => ControlResponse::Session {
            session: directory.check_auth(player).await,
        },

        ControlRequest::SetTransmit { session, transmit } => {
            directory
                .send(DirectoryCommand::SetTransmit { session, transmit })
                .await;
            ControlResponse::Ok
        },

        ControlRequest::PatchPlayer { player, patch } => {
            directory.send(DirectoryCommand::PatchPlayer { player, patch }).await;
            ControlResponse::Ok
        },

        ControlRequest::RemovePlayer { player } => {
            directory.send(DirectoryCommand::RemovePlayer { player }).await;
            ControlResponse::Ok
        },

        ControlRequest::Batch(requests) => {
            if !top_level {
                return ControlResponse::Error {
                    message: "nested batches are not allowed".to_owned(),
                };
            }

            let mut responses = Vec::with_capacity(requests.len());
            for request in requests {
                responses.push(Box::pin(handle(request, directory, false)).await);
            }

            ControlResponse::Batch(responses)
        },
    }
}

/// Errors that can stop the control channel.
#[derive(Debug, Error)]
pub enum Error {
    /// The channel could not be bound, or a client could not be accepted on it.
    #[error(transparent)]
    Channel(#[from] sada_ipc::Error),
    /// The game could not be answered.
    #[error(transparent)]
    Answer(#[from] sada_ipc::SendError),
}

#[cfg(test)]
mod tests {
    use sada_common::{PlayerId, SessionId};
    use tokio::sync::broadcast;

    use super::{ControlEvent, MAX_EVENTS_PER_FRAME, collect};

    /// An event distinguishable from the others by the player it names.
    fn event(player: u32) -> ControlEvent {
        ControlEvent::Disconnected {
            player: PlayerId::from_raw(player),
            session: SessionId::new(1, 1),
        }
    }

    #[tokio::test]
    async fn everything_waiting_travels_in_one_frame() {
        // One event per speaker and listener pair is a lot of events for a crowded room, and a frame each would be a
        // frame flood.
        let (events, mut subscriber) = broadcast::channel(16);

        for player in 1..=4 {
            events.send(event(player)).expect("a subscriber is listening");
        }

        let batch = collect(subscriber.recv().await, &mut subscriber);

        assert_eq!(batch, (1..=4).map(event).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn a_frame_holds_no_more_than_the_cap() {
        let (events, mut subscriber) = broadcast::channel(MAX_EVENTS_PER_FRAME * 2);

        for player in 0..MAX_EVENTS_PER_FRAME as u32 + 10 {
            events.send(event(player + 1)).expect("a subscriber is listening");
        }

        let batch = collect(subscriber.recv().await, &mut subscriber);

        assert_eq!(batch.len(), MAX_EVENTS_PER_FRAME);
    }

    #[tokio::test]
    async fn falling_behind_loses_events_rather_than_the_connection() {
        let (events, mut subscriber) = broadcast::channel(2);

        for player in 1..=4 {
            events.send(event(player)).expect("a subscriber is listening");
        }

        // The first two are gone, and what the subscriber does have still has to reach the game.
        let batch = collect(subscriber.recv().await, &mut subscriber);

        assert_eq!(batch, vec![event(3), event(4)]);
    }
}
