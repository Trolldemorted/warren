//! Trust-dialog auto-accept (§A.7 production path).
//!
//! On a fresh workdir (e.g. a newly-mounted PVC in a k8s pod), the first
//! `claude` launch shows a "Do you trust the files in this folder?" dialog and
//! blocks on it — swallowing any prompt we paste as its yes/no answer. In an
//! unattended supervisor there is no human to press Enter, so the agent would
//! hang forever on first boot.
//!
//! The `claude_smoke` integration test proved the fix works interactively:
//! detect the dialog in the PTY output and send `Enter` to accept it. This
//! module promotes that logic into a production-usable, unit-tested watcher and
//! is the single source of truth for the marker strings (the smoke test imports
//! `has_trust_marker` from here).
//!
//! Detection is deliberately conservative: it only fires a bounded number of
//! times (so a false positive in model output can't spew Enters into a live
//! turn), and clears its scan window after each accept so one dialog render
//! yields exactly one keypress.

/// Substrings (matched case-insensitively) that identify claude's first-run
/// trust dialog. Kept broad enough to survive minor wording changes across
/// claude versions but specific enough not to match ordinary model output.
pub const TRUST_MARKERS: &[&str] = &[
    "trust this folder",
    "do you trust",
    "trust the files",
    "trust the contents",
];

/// Returns true if any trust-dialog marker is visible in `text`
/// (case-insensitive).
pub fn has_trust_marker(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    TRUST_MARKERS.iter().any(|m| lower.contains(m))
}

/// The bytes sent to accept the dialog: a carriage return selects the default
/// ("Yes, I trust…") option.
pub const ACCEPT_BYTES: &[u8] = b"\r";

/// Watches a stream of PTY output chunks for the trust dialog and decides when
/// to auto-accept. Stateful because markers can straddle chunk boundaries and
/// because accepts must be budget-limited.
pub struct TrustWatcher {
    /// Rolling tail of recently-seen output, lossily decoded to text.
    window: String,
    /// Cap on `window` length in bytes; older text is dropped on a char
    /// boundary.
    max_window: usize,
    /// Remaining auto-accepts. Bounds blast radius if a marker ever appears in
    /// legitimate output.
    accepts_remaining: u32,
}

impl TrustWatcher {
    /// A watcher that will auto-accept up to `max_accepts` dialogs. A small
    /// value (2–3) covers the real cases (one dialog, occasionally re-shown)
    /// without risking a keystroke storm.
    pub fn new(max_accepts: u32) -> Self {
        Self {
            window: String::new(),
            max_window: 4096,
            accepts_remaining: max_accepts,
        }
    }

    /// Feed one chunk of PTY output. Returns `Some(ACCEPT_BYTES)` if a trust
    /// dialog is now visible and budget remains — the caller should write those
    /// bytes back to the PTY. Returns `None` otherwise.
    pub fn observe(&mut self, chunk: &[u8]) -> Option<&'static [u8]> {
        if self.accepts_remaining == 0 {
            return None;
        }
        self.window.push_str(&String::from_utf8_lossy(chunk));
        self.truncate_front();
        if has_trust_marker(&self.window) {
            self.accepts_remaining -= 1;
            // Clear so the same on-screen dialog text isn't re-matched on the
            // next chunk and doesn't waste the budget.
            self.window.clear();
            return Some(ACCEPT_BYTES);
        }
        None
    }

    fn truncate_front(&mut self) {
        if self.window.len() <= self.max_window {
            return;
        }
        let mut cut = self.window.len() - self.max_window;
        while cut < self.window.len() && !self.window.is_char_boundary(cut) {
            cut += 1;
        }
        self.window.drain(..cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_marker_case_insensitively() {
        assert!(has_trust_marker("Do you trust the files in this folder?"));
        assert!(has_trust_marker("DO YOU TRUST"));
        assert!(!has_trust_marker("normal model output about a folder"));
    }

    #[test]
    fn observe_accepts_once_on_marker() {
        let mut w = TrustWatcher::new(3);
        assert_eq!(w.observe(b"banner text\n"), None);
        assert_eq!(
            w.observe(b"Do you trust the files in this folder?"),
            Some(ACCEPT_BYTES)
        );
    }

    #[test]
    fn observe_reassembles_across_chunk_boundary() {
        let mut w = TrustWatcher::new(3);
        assert_eq!(w.observe(b"...do you tr"), None);
        assert_eq!(w.observe(b"ust the folder"), Some(ACCEPT_BYTES));
    }

    #[test]
    fn observe_clears_window_so_one_render_is_one_accept() {
        let mut w = TrustWatcher::new(3);
        assert_eq!(w.observe(b"do you trust"), Some(ACCEPT_BYTES));
        // Same text still lingering in a follow-up chunk fragment shouldn't
        // re-trigger until a *fresh* full marker appears.
        assert_eq!(w.observe(b" this folder"), None);
    }

    #[test]
    fn observe_respects_accept_budget() {
        let mut w = TrustWatcher::new(1);
        assert_eq!(w.observe(b"do you trust"), Some(ACCEPT_BYTES));
        assert_eq!(w.observe(b"do you trust"), None, "budget exhausted");
    }

    #[test]
    fn zero_budget_never_accepts() {
        let mut w = TrustWatcher::new(0);
        assert_eq!(w.observe(b"do you trust the files"), None);
    }

    #[test]
    fn window_is_bounded() {
        let mut w = TrustWatcher::new(3);
        let big = "x".repeat(100_000);
        assert_eq!(w.observe(big.as_bytes()), None);
        assert!(w.window.len() <= w.max_window);
    }
}
