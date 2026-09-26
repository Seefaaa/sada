//! Cooperative shutdown signalling shared by every long-lived task.

use tokio::sync::watch;

/// Handle used to request shutdown and to observe the request.
///
/// Cloning yields another observer of the same signal, so each task can hold one
/// and [`select!`](tokio::select!) on [`Shutdown::recv`] alongside its own work. Dropping every
/// clone does not trigger shutdown; only [`Shutdown::trigger`] does.
#[derive(Clone, Debug)]
pub struct Shutdown {
    /// Sender kept alive so observers never see the channel close on its own.
    tx: watch::Sender<bool>,
    /// Observer of the shutdown flag.
    rx: watch::Receiver<bool>,
}

impl Shutdown {
    /// Create a signal that has not yet been triggered.
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self { tx, rx }
    }

    /// Request shutdown, waking every observer.
    pub fn trigger(&self) { let _ = self.tx.send(true); }

    /// Whether shutdown has already been requested.
    #[cfg(test)]
    fn is_triggered(&self) -> bool { *self.rx.borrow() }

    /// Resolve once shutdown has been requested.
    ///
    /// Returns immediately if it was already requested before this call, so a
    /// task that starts late still stops immediately.
    pub async fn recv(&mut self) {
        while !*self.rx.borrow_and_update() {
            if self.rx.changed().await.is_err() {
                return;
            }
        }
    }
}

impl Default for Shutdown {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::Shutdown;

    #[tokio::test]
    async fn observers_wake_on_trigger() {
        let shutdown = Shutdown::new();

        let mut observer = shutdown.clone();
        assert!(!shutdown.is_triggered());

        shutdown.trigger();

        observer.recv().await;
        assert!(observer.is_triggered());
    }

    #[test]
    fn late_observer_returns_immediately() {
        use std::{
            pin::pin,
            task::{Context, Waker},
        };

        let shutdown = Shutdown::new();
        shutdown.trigger();

        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        let poll = pin!(shutdown.clone().recv()).poll(&mut cx);

        assert!(poll.is_ready());
    }
}
