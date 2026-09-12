//! Unix socket control channel for the BYOND game bridge.
//!
//! The game drives this: it sends one request and reads one response, because `call_ext` blocks the game thread and the
//! server can never push to it. Server to game traffic therefore goes through a queue the game drains with
//! [`ControlRequest::PollEvents`].

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use sada_common::{ControlFrameBuffer, ControlRequest, ControlResponse, PROTOCOL_VERSION};
use thiserror::Error;
use tokio::{
    fs,
    net::{UnixListener, UnixStream},
    sync::Semaphore,
};

use crate::{
    directory::{DirectoryCommand, DirectoryHandle},
    shutdown::Shutdown,
};

/// Maximum number of simultaneous control clients.
///
/// The game opens one connection; the rest of the budget is for a debugging tool, a reconnect racing a stale connection
/// or whatever.
const MAX_CLIENTS: usize = 4;

/// Largest number of events returned in one poll, whatever the game asks for.
const MAX_EVENTS_PER_POLL: usize = 256;

/// Run the control listener until shutdown.
pub async fn serve(path: PathBuf, directory: DirectoryHandle, mut shutdown: Shutdown) -> Result<(), Error> {
    remove_stale_socket(&path).await?;

    let listener = UnixListener::bind(&path).map_err(|s| Error::Bind(path.clone(), s))?;
    info!(path = %path.display(), "control socket listening");

    let semaphore = Arc::new(Semaphore::new(MAX_CLIENTS));

    loop {
        let accepted = tokio::select! {
            () = shutdown.recv() => break,
            accepted = listener.accept() => accepted,

        };
        let (stream, _) = accepted.map_err(Error::Accept)?;

        let Ok(permit) = semaphore.clone().acquire_owned().await else {
            break;
        };

        let directory = directory.clone();

        tokio::spawn(async move {
            let _permit = permit;
            if let Err(err) = serve_client(stream, directory).await {
                warn!(?err, "control connection ended with an error");
            }
        });
    }

    let _ = fs::remove_file(&path).await;

    info!("control socket stopped");

    Ok(())
}

/// Remove a socket left behind by a previous process.
async fn remove_stale_socket(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::RemoveStale(path.to_owned(), source)),
    }
}

/// Serve one control client until it disconnects.
async fn serve_client(stream: UnixStream, directory: DirectoryHandle) -> Result<(), Error> {
    let (mut reader, mut writer) = stream.into_split();
    let mut buffer = ControlFrameBuffer::new();

    loop {
        let response = match buffer.read_async(&mut reader).await {
            Ok(Some(request)) => handle(request, &directory, true).await,
            Ok(None) => break, // eof
            Err(err) => ControlResponse::Error {
                message: format!("invalid request: {err}"),
            },
        };

        buffer.write_async(&mut writer, &response).await?;
    }

    Ok(())
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

        ControlRequest::RegisterCode { code, ckey } => {
            directory.send(DirectoryCommand::RegisterCode { code, ckey }).await;
            ControlResponse::Ok
        },

        ControlRequest::CheckAuth { ckey } => ControlResponse::Session {
            session: directory.check_auth(ckey).await,
        },

        ControlRequest::SetTransmit { session, transmit } => {
            directory
                .send(DirectoryCommand::SetTransmit { session, transmit })
                .await;
            ControlResponse::Ok
        },

        ControlRequest::PatchPlayer { ckey, patch } => {
            directory.send(DirectoryCommand::PatchPlayer { ckey, patch }).await;
            ControlResponse::Ok
        },

        ControlRequest::RemovePlayer { ckey } => {
            directory.send(DirectoryCommand::RemovePlayer { ckey }).await;
            ControlResponse::Ok
        },

        ControlRequest::PollEvents { max } => {
            let max = usize::from(max).min(MAX_EVENTS_PER_POLL);
            ControlResponse::Events(directory.poll_events(max).await)
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
    /// A socket left by an earlier process could not be removed.
    #[error("failed to remove the stale control socket at {0}")]
    RemoveStale(PathBuf, #[source] io::Error),
    /// The listener could not bind.
    #[error("failed to bind the control socket at {0}")]
    Bind(PathBuf, #[source] io::Error),
    /// A client connection could not be accepted.
    #[error("failed to accept a control connection")]
    Accept(#[source] io::Error),
    /// Shared control frame protocol error.
    #[error(transparent)]
    Frame(#[from] sada_common::Error),
}
