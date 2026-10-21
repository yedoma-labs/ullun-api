//! Shutdown signal and background tasks
//!
//! When the server is asked to stop (CTRL+C, SIGTERM, or the future given to
//! `App::serve_with_shutdown` completes) it stops accepting connections, tells
//! idle keep-alive connections to close, lets in-flight requests finish, and
//! waits for [background tasks](crate::app::App::background), each at most
//! `App::shutdown_grace` (default 10 s). Long-lived work such as SSE streams,
//! WebSocket handlers and periodic jobs should watch [`Shutdown`] to end
//! promptly instead of running into that limit.

use std::time::Duration;
use tokio::sync::watch;

/// Resolves when the server begins shutting down
#[derive(Clone)]
pub struct Shutdown {
    rx: watch::Receiver<bool>,
    drain: watch::Receiver<bool>,
}

impl Shutdown {
    pub(crate) fn new(rx: watch::Receiver<bool>) -> Self {
        // no separate drain phase: draining begins together with shutdown
        let (tx, drain) = watch::channel(false);
        std::mem::forget(tx);
        Self { rx, drain }
    }

    pub(crate) fn with_drain(rx: watch::Receiver<bool>, drain: watch::Receiver<bool>) -> Self {
        Self { rx, drain }
    }

    /// A signal that never fires (requests built outside a running server)
    pub fn never() -> Self {
        let (tx, rx) = watch::channel(false);
        // keep the sender alive for the process lifetime so `wait` pends instead of erroring
        std::mem::forget(tx);
        Self::new(rx)
    }

    /// True once the server was told to stop, which is up to
    /// `App::shutdown_delay` before [`is_triggered`](Shutdown::is_triggered).
    /// Readiness checks report "not ready" from here on so load balancers stop
    /// sending traffic while the server still answers.
    pub fn is_draining(&self) -> bool {
        *self.drain.borrow() || self.is_triggered()
    }

    /// True once shutdown has begun
    pub fn is_triggered(&self) -> bool {
        *self.rx.borrow()
    }

    /// Completes when shutdown begins (immediately if it already has)
    pub async fn wait(&self) {
        let mut rx = self.rx.clone();
        loop {
            if *rx.borrow_and_update() {
                return;
            }
            if rx.changed().await.is_err() {
                // sender gone without a signal: the server is being torn down
                return;
            }
        }
    }

    /// Sleep for `duration`; returns `false` if shutdown began first
    ///
    /// Designed for loops: `while shutdown.sleep(Duration::from_secs(30)).await { work().await }`.
    pub async fn sleep(&self, duration: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(duration) => !self.is_triggered(),
            _ = self.wait() => false,
        }
    }
}

/// Handed to every task registered with `App::background`; cheap to clone into
/// sub-tasks
#[derive(Clone)]
pub struct Context {
    pub(crate) state: std::sync::Arc<crate::request::StateMap>,
    shutdown: Shutdown,
}

impl Context {
    pub(crate) fn new(state: std::sync::Arc<crate::request::StateMap>, shutdown: Shutdown) -> Self {
        Self { state, shutdown }
    }

    /// Shared state registered with `App::state`
    pub fn state<T: Send + Sync + 'static>(&self) -> crate::error::Result<&T> {
        self.state.get::<T>().ok_or_else(|| {
            crate::error::Error::internal(format!(
                "State of type {} is not registered",
                std::any::type_name::<T>()
            ))
        })
    }

    /// The shutdown signal; clone it into spawned sub-tasks
    pub fn shutdown(&self) -> Shutdown {
        self.shutdown.clone()
    }

    /// Shorthand for `shutdown().sleep(duration)`
    pub async fn sleep(&self, duration: Duration) -> bool {
        self.shutdown.sleep(duration).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_sleep_and_trigger() {
        let (tx, rx) = watch::channel(false);
        let s = Shutdown::new(rx);
        assert!(!s.is_triggered());
        assert!(s.sleep(Duration::from_millis(5)).await);

        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.wait().await });
        tx.send(true).unwrap();
        waiter.await.unwrap();
        assert!(s.is_triggered());
        // already triggered: returns at once, sleep reports the interruption
        s.wait().await;
        assert!(!s.sleep(Duration::from_secs(60)).await);
    }

    #[tokio::test]
    async fn sleep_is_interrupted_by_the_signal() {
        let (tx, rx) = watch::channel(false);
        let s = Shutdown::new(rx);
        let sleeper = tokio::spawn({
            let s = s.clone();
            async move { s.sleep(Duration::from_secs(60)).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(true).unwrap();
        assert!(!tokio::time::timeout(Duration::from_secs(5), sleeper)
            .await
            .unwrap()
            .unwrap());
    }

    #[test]
    fn draining_precedes_or_equals_shutdown() {
        let (tx, rx) = watch::channel(false);
        let (dtx, drx) = watch::channel(false);
        let s = Shutdown::with_drain(rx, drx);
        assert!(!s.is_draining());
        dtx.send(true).unwrap();
        assert!(s.is_draining() && !s.is_triggered());
        tx.send(true).unwrap();
        assert!(s.is_triggered());

        // without a drain phase, draining starts with shutdown
        let (tx, rx) = watch::channel(false);
        let s = Shutdown::new(rx);
        assert!(!s.is_draining());
        tx.send(true).unwrap();
        assert!(s.is_draining());
    }

    #[tokio::test]
    async fn never_does_not_fire() {
        let s = Shutdown::never();
        assert!(!s.is_triggered());
        assert!(tokio::time::timeout(Duration::from_millis(30), s.wait())
            .await
            .is_err());
    }
}
