//! Voice server for Space Station 13.
//!
//! The process is built from three long-lived actors that only ever talk to each
//! other over channels:
//!
//! * the **SFU worker** owns the UDP socket and every [`str0m::Rtc`] instance,
//! * the **directory** owns player identity and the audio routing policy,
//! * the **control channel** relays commands and events to the BYOND game server.
//!
//! Browser connections get one task each, which does WebSocket framing.

#[macro_use]
extern crate tracing;

#[cfg(feature = "audio_dump")]
mod audio;
mod config;
mod control;
mod directory;
mod http;
mod proto;
mod sfu;
mod shutdown;
mod ws;

use std::{net::SocketAddr, sync::Arc};

use thiserror::Error;
use tokio::{net::TcpListener, sync::mpsc};
use tracing_subscriber::EnvFilter;

use crate::{config::Config, directory::Directory, http::AppState, sfu::Worker, shutdown::Shutdown};

/// How many SFU notifications may be queued for the directory.
const SFU_EVENT_BUFFER: usize = 64;

/// Result type used by the server entry point.
type Result<T> = std::result::Result<T, Error>;

/// Server entry point.
#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config = Arc::new(Config::load_from_env()?);

    let shutdown = Shutdown::new();

    let (socket, media_addr) = Worker::bind(&config)?;

    let (sfu_events, sfu_event_rx) = mpsc::channel(SFU_EVENT_BUFFER);
    let worker = Worker::spawn(socket, media_addr, sfu_events, shutdown.clone());

    let directory = Directory::spawn(&config, worker.clone(), sfu_event_rx, shutdown.clone());

    if let Some(path) = &config.server.control_socket {
        let path = path.clone();
        let directory = directory.clone();
        let shutdown = shutdown.clone();

        tokio::spawn(async move {
            if let Err(err) = control::serve(path, directory, shutdown).await {
                error!(?err, "control socket stopped");
            }
        });
    }

    let addr = config.server.listen;
    let state = AppState {
        config,
        worker,
        directory,
    };

    let listener = TcpListener::bind(addr).await.map_err(|s| Error::BindHttp(addr, s))?;
    info!(%addr, "http server listening");

    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(async move {
            wait_for_signal().await;
            info!("shutdown requested");
            shutdown.trigger();
        })
        .await
        .map_err(Error::ServeHttp)?;

    Ok(())
}

/// Install the tracing subscriber, defaulting this crate to debug level.
fn init_tracing() {
    let filter = EnvFilter::from_default_env().add_directive(
        "sada_server=debug"
            .parse()
            .expect("the built-in log directive is always valid"),
    );
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Resolve once the process is asked to terminate.
async fn wait_for_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        error!(?err, "failed to listen for termination signal");
    }
}

/// Errors that can stop the server process.
#[derive(Debug, Error)]
pub enum Error {
    /// The server configuration could not be loaded.
    #[error(transparent)]
    Config(#[from] config::Error),
    /// The media socket could not be prepared.
    #[error(transparent)]
    Sfu(#[from] sfu::Error),
    /// The HTTP listener could not bind to the configured address.
    #[error("failed to bind HTTP listener at {0}")]
    BindHttp(SocketAddr, #[source] std::io::Error),
    /// The HTTP server stopped with an I/O error.
    #[error("HTTP server error")]
    ServeHttp(#[source] std::io::Error),
}
