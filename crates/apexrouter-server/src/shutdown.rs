//! OWNER: unit S-01 (server/src/{lib,state,shutdown}.rs). Do not edit outside that unit.
//!
//! Graceful shutdown.
//!
//! `tokio::signal` sets a flag; both listeners stop accepting; in-flight requests drain to
//! `drain_timeout_secs`; ledger and usage appends complete inside awaited tasks. `SIGHUP`
//! **reloads config instead of exiting**. The lock is released by process exit.
//!
//! Two things shutdown deliberately does **not** do: it never signals a `llama-server`
//! child (they are `setsid`, and `kill_children_on_exit` defaults to false), and it **never
//! destroys a vast instance, at any setting** — a crash must not delete a paid box.
//!
//! The flag is a `tokio::sync::watch<bool>` rather than a `Notify` or a oneshot, because
//! both listeners *and* the `SIGHUP` loop have to observe the same edge, a late subscriber
//! must still see a trigger that already happened, and `axum::serve` wants a plain future.
//! A dropped [`Shutdown`] reads as "triggered": nobody can ever fire it again, so waiting
//! forever would be a hang rather than a shutdown.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// The trigger side of the shutdown flag. Cloneable; any clone may fire it.
#[derive(Clone, Debug)]
pub struct Shutdown {
    tx: Arc<watch::Sender<bool>>,
}

/// The observer side. Every listener holds one and awaits [`ShutdownHandle::wait`].
#[derive(Clone, Debug)]
pub struct ShutdownHandle {
    rx: watch::Receiver<bool>,
}

/// A fresh, untriggered shutdown flag.
pub fn channel() -> (Shutdown, ShutdownHandle) {
    let (tx, rx) = watch::channel(false);
    (Shutdown { tx: Arc::new(tx) }, ShutdownHandle { rx })
}

impl Shutdown {
    /// Fire it. Idempotent: a second call is a no-op, not an error.
    pub fn trigger(&self) {
        // A send failure means every receiver is gone, which is exactly as good as a
        // delivered shutdown.
        let _ = self.tx.send(true);
    }

    /// Another observer.
    pub fn handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            rx: self.tx.subscribe(),
        }
    }
}

impl ShutdownHandle {
    /// Resolve once shutdown has been triggered — including when it was triggered before
    /// this handle started waiting.
    pub async fn wait(&self) {
        let mut rx = self.rx.clone();
        // `borrow_and_update`'s guard is dropped at the end of the condition expression, so
        // nothing is held across the await.
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                // Every sender is gone: treat it as triggered rather than hanging.
                return;
            }
        }
    }

    /// Has it fired already? For a handler that wants to answer `503` while draining.
    pub fn is_triggered(&self) -> bool {
        *self.rx.borrow()
    }
}

/// Wait for `SIGINT` or `SIGTERM` and return which one arrived.
///
/// `SIGHUP` is deliberately absent: it means *reload*, and is handled by the daemon's own
/// loop. If either handler cannot be installed the future never resolves, which leaves the
/// daemon running rather than exiting for a reason nobody asked for.
pub async fn wait_for_terminate() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigint = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot install a SIGINT handler");
            std::future::pending::<()>().await;
            return "never";
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot install a SIGTERM handler");
            std::future::pending::<()>().await;
            return "never";
        }
    };

    tokio::select! {
        _ = sigint.recv()  => "SIGINT",
        _ = sigterm.recv() => "SIGTERM",
    }
}

/// Await both listener tasks, but no longer than `deadline`.
///
/// Returns `true` when everything in flight finished inside the budget. `false` means the
/// deadline expired and the remaining connections are being dropped — which is the honest
/// outcome to log, not an error to propagate: the operator asked the process to stop.
///
/// The tasks are **not** aborted on expiry. The process is about to exit and take them with
/// it; aborting a task mid-`ledger.append()` is how a shutdown loses a usage row.
pub async fn drain(handles: Vec<JoinHandle<std::io::Result<()>>>, deadline: Duration) -> bool {
    let all = async {
        for h in handles {
            match h.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!(error = %e, "listener stopped with an error"),
                Err(e) => tracing::warn!(error = %e, "listener task did not join cleanly"),
            }
        }
    };
    tokio::time::timeout(deadline, all).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_handle_that_starts_waiting_late_still_sees_the_trigger() {
        let (sd, handle) = channel();
        sd.trigger();
        // Already fired: this must resolve immediately, not hang.
        tokio::time::timeout(Duration::from_secs(1), handle.wait())
            .await
            .expect("wait() must resolve for an already-triggered flag");
        assert!(handle.is_triggered());
    }

    #[tokio::test]
    async fn every_handle_observes_one_trigger() {
        let (sd, a) = channel();
        let b = sd.handle();
        let c = a.clone();
        assert!(!a.is_triggered());
        sd.trigger();
        for h in [a, b, c] {
            tokio::time::timeout(Duration::from_secs(1), h.wait())
                .await
                .expect("every handle must observe the trigger");
        }
    }

    #[tokio::test]
    async fn dropping_every_sender_reads_as_shutdown_rather_than_a_hang() {
        let (sd, handle) = channel();
        drop(sd);
        tokio::time::timeout(Duration::from_secs(1), handle.wait())
            .await
            .expect("a dropped trigger must not hang the listeners");
    }

    #[tokio::test]
    async fn drain_reports_whether_the_deadline_was_met() {
        let quick: JoinHandle<std::io::Result<()>> = tokio::spawn(async { Ok(()) });
        assert!(drain(vec![quick], Duration::from_secs(5)).await);

        let slow: JoinHandle<std::io::Result<()>> = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        });
        assert!(!drain(vec![slow], Duration::from_millis(50)).await);
    }
}
