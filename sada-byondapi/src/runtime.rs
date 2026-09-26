//! The tokio runtime async exports run on.
//!
//! BYOND has one thread and `call_ext` blocks it, so an async export returns immediately and finishes its work here,
//! answering the waiting proc through [`Byond_ThreadSync`](crate::sys::Byondapi::Byond_ThreadSync) when it is done.

use std::{
    sync::{OnceLock, mpsc},
    thread,
};

use sada_utils::shutdown::Shutdown;
use tokio::runtime;

/// Handle to the runtime, once its thread has started.
static HANDLE: OnceLock<runtime::Handle> = OnceLock::new();
/// Signal that ends the runtime thread.
static SHUTDOWN: OnceLock<Shutdown> = OnceLock::new();

/// The runtime, starting its thread on first use.
///
/// # Panics
///
/// If the thread or the runtime cannot be created, which is a dead library either way.
pub fn runtime() -> &'static runtime::Handle {
    HANDLE.get_or_init(|| {
        let (tx, rx) = mpsc::channel();

        thread::Builder::new()
            .name("sada-runtime".to_owned())
            .spawn(move || {
                let runtime = runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to initialize tokio runtime");

                let handle = runtime.handle().clone();

                if tx.send(handle).is_err() {
                    return;
                }

                let mut shutdown = SHUTDOWN.get_or_init(Shutdown::new).clone();

                runtime.block_on(async move {
                    shutdown.recv().await;
                });
            })
            .expect("failed to spawn runtime thread");

        rx.recv().expect("runtime thread panicked before sending handle")
    })
}

/// Start the runtime ahead of the first async call, for any reason.
pub fn initialize() { let _ = runtime(); }

/// Stop the runtime, if it was ever started.
pub fn shutdown() {
    if let Some(shutdown) = SHUTDOWN.get() {
        shutdown.trigger()
    }
}
