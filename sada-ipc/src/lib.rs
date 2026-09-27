//! Typed channels between two processes on one machine, over a unix socket or a named pipe.
//!
//! A channel is used the way the channels in the standard library are: one call hands back the two ends of it, one end
//! sends values and the other receives them, and both ends are cloneable.
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let endpoint = sada_ipc::Endpoint::new("/tmp/example.sock");
//!
//! // the process that listens
//! let mut listener = sada_ipc::Listener::bind(&endpoint)?;
//! let (to_client, from_client) = listener.accept::<String, u32>().await?;
//!
//! // the process that connects
//! let (to_server, from_server) = sada_ipc::connect::<u32, String>(&endpoint).await?;
//!
//! to_server.send_async(1).await?;
//! let request: u32 = from_client.recv_async().await?;
//! # Ok(())
//! # }
//! ```
//!
//! The transport under it is a unix socket where there is one and a named pipe on windows; [`Endpoint`] is the one
//! address both hosts read. Values cross as length-prefixed [`postcard`] frames, and the transport is
//! driven by a task of its own in each direction, which is what lets the ends be cloned and lets a send return without
//! waiting on the peer. Each end has both a blocking and an asynchronous way of being used, so a thread that is not
//! running a runtime, such as a game engine's, can hold one too.
//!
//! Nothing here reconnects: a channel belongs to one connection, and whoever wants another one after it closes calls
//! [`connect`] again.

mod channel;
mod endpoint;
mod frame;
mod listener;
mod transport;

use std::io;

use serde::de::DeserializeOwned;
use thiserror::Error;

pub use crate::{
    channel::{Receiver, RecvError, SendError, Sender},
    endpoint::Endpoint,
    frame::{DEFAULT_MAX_FRAME_LEN, Error as FrameError},
    listener::Listener,
};

/// Result type used while setting a channel up.
pub type Result<T> = std::result::Result<T, Error>;

/// How many frames a channel queues in each direction before a send has to wait.
const DEFAULT_CAPACITY: usize = 256;

/// What a channel is built with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelConfig {
    /// Largest frame payload the channel sends or accepts.
    ///
    /// A frame the peer announces as larger than this ends the channel; a value that encodes larger than this is
    /// refused without touching it.
    pub max_frame_len: u32,
    /// How many frames it queues in each direction before a send has to wait.
    pub capacity: usize,
}

impl ChannelConfig {
    /// A configuration with the default queue depth.
    #[must_use]
    pub const fn new(max_frame_len: u32) -> Self {
        Self {
            max_frame_len,
            capacity: DEFAULT_CAPACITY,
        }
    }
}

impl Default for ChannelConfig {
    fn default() -> Self { Self::new(DEFAULT_MAX_FRAME_LEN) }
}

/// Connect to `endpoint`, with the default configuration.
pub async fn connect<Tx, Rx>(endpoint: &Endpoint) -> Result<(Sender<Tx>, Receiver<Rx>)>
where
    Rx: DeserializeOwned + Send + 'static,
{
    connect_with(endpoint, ChannelConfig::default()).await
}

/// Connect to `endpoint`.
///
/// Must be called from within a tokio runtime, which drives the channel for as long as an end of it is held. `Tx` is
/// what this end sends and `Rx` what it receives, which are the listening end's two the other way around.
pub async fn connect_with<Tx, Rx>(endpoint: &Endpoint, config: ChannelConfig) -> Result<(Sender<Tx>, Receiver<Rx>)>
where
    Rx: DeserializeOwned + Send + 'static,
{
    let stream = transport::connect(endpoint)
        .await
        .map_err(|source| Error::Connect(endpoint.clone(), source))?;

    Ok(channel::spawn(stream, config))
}

/// Errors that can keep a channel from being set up, or end one that was.
#[derive(Debug, Error)]
pub enum Error {
    /// The listener could not bind.
    #[error("failed to bind {0}")]
    Bind(Endpoint, #[source] io::Error),
    /// The endpoint could not be reached.
    #[error("failed to connect to {0}")]
    Connect(Endpoint, #[source] io::Error),
    /// A client could not be accepted.
    #[error("failed to accept a connection on {0}")]
    Accept(Endpoint, #[source] io::Error),
    /// A frame could not be moved on or off the transport.
    #[error(transparent)]
    Frame(#[from] FrameError),
}
