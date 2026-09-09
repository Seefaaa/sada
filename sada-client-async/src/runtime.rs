use std::{
    ffi::CString,
    sync::{OnceLock, mpsc},
    thread,
};

use tokio::{runtime, sync::watch};

use crate::{ffi::BYOND, shutdown::Shutdown};

static HANDLE: OnceLock<runtime::Handle> = OnceLock::new();
static SHUTDOWN: OnceLock<Shutdown> = OnceLock::new();

pub fn runtime() -> &'static runtime::Handle {
    HANDLE.get_or_init(|| {
        let (tx, rx) = mpsc::channel();

        thread::Builder::new().name("sada-runtime".to_owned()).spawn(move || {
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
        });

        rx.recv().expect("runtime thread panicked before sending handle")
    })
}

pub fn initialize() { let _ = runtime(); }

pub fn shutdown() {
    if let Some(shutdown) = SHUTDOWN.get() {
        shutdown.trigger()
    }
}
