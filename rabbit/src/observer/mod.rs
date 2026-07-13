pub mod context;
pub mod hooks;
pub mod limits;
pub mod state;
pub mod transcript;

use parking_lot::RwLock;
use rabbit_lib::wire::UsageSnapshot;
use std::sync::OnceLock;

/// §Context-window / §Cross-crate merge: cache of the most recent
/// transcript-derived `UsageSnapshot`. The supervisor's
/// `ContextCheck` arm reads this when building the post-scrape
/// `Usage` envelope so it can layer the modal-derived `ctx_*`
/// fields on top of the dashboard's still-live input / output /
/// cache counters — without re-implementing transcript parsing in
/// the supervisor path.
///
/// Stored in a `parking_lot::RwLock` behind a `OnceLock` so the
/// first writer initializes the slot and subsequent writers
/// replace atomically. Reads (`latest_usage`) return a clone so
/// callers can use the snapshot without holding the lock across an
/// await.
static LATEST_USAGE: OnceLock<RwLock<UsageSnapshot>> = OnceLock::new();

/// Record the latest transcript-derived `UsageSnapshot`. Called
/// from `transcript::spawn_transcript_relay` after every successful
/// JSONL line so the supervisor always sees fresh input/output/
/// cache counters when it builds a `/context` scrape response.
pub fn record_usage(snap: UsageSnapshot) {
    let lock = LATEST_USAGE.get_or_init(|| RwLock::new(UsageSnapshot::default()));
    *lock.write() = snap;
}

/// Returns a clone of the most recently recorded `UsageSnapshot`,
/// or a fresh `Default::default()` if no scrape has happened yet.
pub fn latest_usage() -> UsageSnapshot {
    LATEST_USAGE
        .get()
        .map(|l| l.read().clone())
        .unwrap_or_default()
}