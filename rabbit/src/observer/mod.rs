pub mod context;
pub mod hooks;
pub mod limits;
pub mod state;
pub mod text;
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
///
/// `ctx_*` fields and `ctx_scrape_incomplete` are MERGED, not
/// overwritten: a transcript-derived snapshot carries `None` for
/// the modal fields (the transcript parser doesn't see the
/// `/context` overlay), so a naive overwrite would clobber a
/// freshly-published `/context` result the moment the next
/// JSONL line lands. Merging preserves the modal values across
/// intervening transcript ticks until the next `ContextCheck`
/// republishes them.
pub fn record_usage(snap: UsageSnapshot) {
    let lock = LATEST_USAGE.get_or_init(|| RwLock::new(UsageSnapshot::default()));
    let mut g = lock.write();
    let mut merged = snap;
    if merged.ctx_used_tokens.is_none() {
        merged.ctx_used_tokens = g.ctx_used_tokens.take();
    }
    if merged.ctx_total_tokens.is_none() {
        merged.ctx_total_tokens = g.ctx_total_tokens.take();
    }
    if merged.ctx_used_pct.is_none() {
        merged.ctx_used_pct = g.ctx_used_pct.take();
    }
    if merged.ctx_free_pct.is_none() {
        merged.ctx_free_pct = g.ctx_free_pct.take();
    }
    if merged.ctx_window_tokens.is_none() {
        merged.ctx_window_tokens = g.ctx_window_tokens.take();
    }
    if merged.ctx_categories.is_none() {
        merged.ctx_categories = g.ctx_categories.take();
    }
    merged.ctx_scrape_incomplete |= g.ctx_scrape_incomplete;
    *g = merged;
}

/// Returns a clone of the most recently recorded `UsageSnapshot`,
/// or a fresh `Default::default()` if no scrape has happened yet.
pub fn latest_usage() -> UsageSnapshot {
    LATEST_USAGE
        .get()
        .map(|l| l.read().clone())
        .unwrap_or_default()
}
