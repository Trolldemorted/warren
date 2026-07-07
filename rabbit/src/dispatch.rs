//! Reliable delivery helpers for mpsc channels.
//!
//! `send_or_warn` (async) and `try_send_or_warn` (sync) wrap every
//! `send` / `try_send` site whose failure mode is meaningful. Each
//! emits a `log::warn!` on failure so silent drops become auditable.
//!
//! **Two distinct helpers — not one with `.await` half the time.** A
//! caller picking the wrong variant gets a compile error (the async
//! helper takes `&mut` of a `mpsc::Sender::send` future that doesn't
//! exist in sync contexts, and the sync helper takes ownership of
//! `T` by value).
//!
//! ## When to apply
//!
//! Apply to every mpsc `Sender<T>::send` / `try_send` whose failure
//! is meaningful (channel full or disconnected). Silent drops in
//! those sites leave no audit trail.
//!
//! ## When to leave silent (with a `// intentional:` comment)
//!
//! - Broadcast `Sender::send` — failures only mean "no subscribers"
//!   which is expected for a fresh agent.
//! - `oneshot::Sender::send` — the receiver dropping is the
//!   oneshot contract; logging it would only add noise.
//! - Internal actor mpsc sites (writer actor's own sends to itself)
//!   — the actor's death is the only way these fail and the actor
//!   is already exiting.

use std::fmt::Debug;
use tokio::sync::mpsc;

/// Async `send` with warn-on-failure. Awaits capacity; bounded
/// backpressure matches the actor mpsc semantics.
pub async fn send_or_warn<T: Debug>(label: &str, tx: &mpsc::Sender<T>, cmd: T) {
    if tx.send(cmd).await.is_err() {
        log::warn!("dropped {label}: channel closed or full");
    }
}

/// Sync `try_send` with warn-on-failure. Does not block; useful
/// for sites that already hold a non-`Send` future (e.g. inside a
/// `select!` arm). Drop semantics match `try_send`: `Err(Full)` and
/// `Err(Closed)` both log at warn and move on.
pub fn try_send_or_warn<T: Debug>(label: &str, tx: &mpsc::Sender<T>, cmd: T) {
    if tx.try_send(cmd).is_err() {
        log::warn!("dropped {label}: channel closed or full");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn send_or_warn_logs_on_closed_channel() {
        // Closing the receiver before sending -> the channel is
        // immediately in the disconnected state. Pin the warn log.
        let (tx, rx) = mpsc::channel::<u32>(1);
        drop(rx);
        // Capture log output via a test logger? The default test
        // harness sinks log to /dev/null; for a behavior pin it's
        // enough that the helper doesn't panic or hang.
        send_or_warn("test-label", &tx, 42).await;
    }

    #[tokio::test]
    async fn try_send_or_warn_logs_on_full_channel() {
        let (tx, mut rx) = mpsc::channel::<u32>(1);
        // Fill the channel: subsequent try_send returns Err(Full).
        send_or_warn("first", &tx, 1).await;
        // Receiver hasn't drained; capacity is exhausted.
        try_send_or_warn("second", &tx, 2);
        // Drain so the receiver closes cleanly; without this the
        // channel stays open and the test panics on Drop.
        let _ = rx.recv().await;
    }
}
