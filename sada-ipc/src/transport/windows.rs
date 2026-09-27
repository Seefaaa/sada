//! Named pipe transport.

use std::{
    io,
    time::{Duration, Instant},
};

use tokio::{
    net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions},
    time::sleep,
};

use crate::Endpoint;

/// `ERROR_PIPE_BUSY`, which says the pipe exists but every instance of it is taken.
///
/// Compared as a raw code rather than through a bindings crate, which this is the only use for.
const ERROR_PIPE_BUSY: i32 = 231;

/// How long [`connect`] keeps trying a busy pipe before giving the error to the caller.
const BUSY_RETRY_WINDOW: Duration = Duration::from_secs(1);

/// How long [`connect`] waits between tries at a busy pipe.
const BUSY_RETRY_DELAY: Duration = Duration::from_millis(50);

/// A named pipe, accepting connections.
///
/// A pipe has no listening handle separate from the connections it accepts: the server keeps one unconnected instance,
/// hands it to the client that arrives and creates the next one. Creating the next instance before handing the
/// accepted one back is what keeps a client that arrives in between from finding no pipe at all.
#[derive(Debug)]
pub struct Listener {
    /// The instance waiting for the next client, absent when the last accept could not make one.
    pending: Option<NamedPipeServer>,
    /// Name of the pipe, kept for creating further instances.
    name: String,
}

impl Listener {
    /// Create the first instance of the pipe.
    pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
        // first_pipe_instance so another process cannot quietly serve the same name alongside us
        let pending = ServerOptions::new()
            .first_pipe_instance(true)
            .create(endpoint.as_str())?;

        Ok(Self {
            pending: Some(pending),
            name: endpoint.as_str().to_owned(),
        })
    }

    /// Wait for the next client.
    pub async fn accept(&mut self) -> io::Result<NamedPipeServer> {
        let pending = match self.pending.take() {
            Some(pending) => pending,
            None => ServerOptions::new().create(&self.name)?,
        };

        pending.connect().await?;

        self.pending = Some(ServerOptions::new().create(&self.name)?);

        Ok(pending)
    }
}

/// Connect to a pipe, waiting out a busy one.
///
/// Every other failure, a pipe that is not there included, is the caller's to deal with.
pub async fn connect(endpoint: &Endpoint) -> io::Result<NamedPipeClient> {
    let deadline = Instant::now() + BUSY_RETRY_WINDOW;

    loop {
        match ClientOptions::new().open(endpoint.as_str()) {
            Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => {
                sleep(BUSY_RETRY_DELAY).await;
            },
            other => return other,
        }
    }
}
