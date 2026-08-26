//! — server-side virtual terminal state (Phase A).
//!
//! [`TermTracker`] is a passive observer on the claude PTY byte stream. It
//! feeds every read chunk into an [`avt::Vt`] (asciinema's virtual terminal)
//! so the supervisor always holds an authoritative screen + cursor state. A
//! future phase serializes [`TermTracker::snapshot`] into a `ScreenSnapshot`
//! wire envelope, letting a late browser joiner receive a precise screen dump
//! instead of relying on the SIGWINCH "jiggle" heuristic.
//!
//! ## Why the UTF-8 feeder
//!
//! `avt::Vt::feed_str` takes `&str`, but the PTY hands us raw bytes in ~4 KiB
//! chunks, and a multibyte UTF-8 codepoint (or a wide grapheme) can straddle a
//! chunk boundary. Feeding a chunk that ends mid-codepoint would either lose
//! bytes or corrupt the character. [`TermTracker::feed`] therefore buffers the
//! incomplete trailing bytes and prepends them to the next chunk, so the VT
//! only ever sees whole codepoints.

use avt::Vt;

/// An authoritative screen dump for a late joiner: the visible grid plus the
/// cursor. Row count equals `rows`; each row string is the terminal's own
/// space-padded line text.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // consumed by the Phase B `ScreenSnapshot` wire envelope.
pub struct ScreenSnapshot {
    pub cols: u16,
    pub rows: u16,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub cursor_visible: bool,
    /// Merged view: avt `Buffer::text()` collapses consecutive
    /// `wrapped=true` rows into one Vec slot. Kept for backwards
    /// compat with the v1 wire shape and as a fallback.
    pub text: Vec<String>,
    /// Per-visible-row view: one Vec entry per physical row, no merge.
    /// `vt.view()` skips scrollback and yields each `wrapped` row as
    /// its own entry, so the browser's per-row apply can place each
    /// string at its own xterm row via CSI positioning. Cleared rows
    /// stay cleared; the wrap-continuation tail that avt's `text()`
    /// would have absorbed into the previous entry is preserved here
    /// until a TUI explicitly clears it.
    pub physical_rows: Vec<String>,
}

/// Passive VT observer over the PTY byte stream.
pub struct TermTracker {
    vt: Vt,
    /// Trailing bytes of an incomplete UTF-8 sequence carried to the next feed.
    pending: Vec<u8>,
}

impl TermTracker {
    pub fn new(cols: u16, rows: u16, scrollback_limit: usize) -> Self {
        let vt = Vt::builder()
            .size(cols as usize, rows as usize)
            .scrollback_limit(scrollback_limit)
            .build();
        Self {
            vt,
            pending: Vec::new(),
        }
    }

    /// Feed a raw PTY chunk into the VT. Incomplete trailing UTF-8 sequences
    /// are buffered until the next call; genuinely invalid bytes are replaced
    /// with U+FFFD so a malformed stream can never wedge the tracker.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.pending.extend_from_slice(chunk);
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    if !s.is_empty() {
                        self.vt.feed_str(s);
                    }
                    self.pending.clear();
                    return;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        let s = std::str::from_utf8(&self.pending[..valid])
                            .expect("valid_up_to prefix is valid utf-8");
                        self.vt.feed_str(s);
                    }
                    match e.error_len() {
                        // Incomplete trailing sequence: keep the tail for the
                        // next chunk, drop the prefix we just fed.
                        None => {
                            self.pending.drain(..valid);
                            return;
                        }
                        // A truly invalid sequence of `bad` bytes: emit a
                        // replacement char, skip past it, and keep decoding.
                        Some(bad) => {
                            self.vt.feed_str("\u{FFFD}");
                            self.pending.drain(..valid + bad);
                        }
                    }
                }
            }
        }
    }

    /// Track a terminal resize so the VT grid matches the PTY winsize. Called
    /// on `PtyCmd::Resize` (not on the transient jiggle, which restores the
    /// original size).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.vt.resize(cols as usize, rows as usize);
    }

    /// Current screen + cursor as an owned snapshot for a late joiner.
    #[allow(dead_code)] // consumed by the Phase B `ScreenSnapshot` wire path.
    pub fn snapshot(&self) -> ScreenSnapshot {
        let cursor = self.vt.cursor();
        let (cols, rows) = self.vt.size();
        let physical_rows: Vec<String> = self.vt.view().map(|line| line.text()).collect();
        ScreenSnapshot {
            cols: cols as u16,
            rows: rows as u16,
            cursor_col: cursor.col as u16,
            cursor_row: cursor.row as u16,
            cursor_visible: cursor.visible,
            text: self.vt.text(),
            physical_rows,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_lands_on_the_grid() {
        let mut t = TermTracker::new(20, 5, 100);
        t.feed(b"hello\r\nworld");
        let snap = t.snapshot();
        assert_eq!(snap.cols, 20);
        assert_eq!(snap.rows, 5);
        assert_eq!(snap.text[0].trim_end(), "hello");
        assert_eq!(snap.text[1].trim_end(), "world");
        // Cursor sits just past "world" on the second row.
        assert_eq!(snap.cursor_row, 1);
        assert_eq!(snap.cursor_col, 5);
    }

    #[test]
    fn utf8_codepoint_split_across_chunks_is_reassembled() {
        // "é" is 0xC3 0xA9. Split it across two feeds — the naive approach
        // (decode each chunk independently) would drop or corrupt it. The
        // feeder must buffer the lone 0xC3 and complete it on the next call.
        let mut t = TermTracker::new(10, 2, 100);
        t.feed(&[0xC3]);
        // Nothing decodable yet; the byte is buffered, screen still blank.
        assert_eq!(t.snapshot().text[0].trim_end(), "");
        t.feed(&[0xA9]);
        assert_eq!(t.snapshot().text[0].trim_end(), "é");
    }

    #[test]
    fn multibyte_emoji_split_three_ways_reassembles() {
        // "😀" is 4 bytes: F0 9F 98 80. Feed one byte at a time.
        let bytes = "😀".as_bytes().to_vec();
        let mut t = TermTracker::new(10, 2, 100);
        for b in &bytes {
            t.feed(&[*b]);
        }
        assert_eq!(t.snapshot().text[0].trim_end(), "😀");
    }

    #[test]
    fn clear_and_home_repositions() {
        let mut t = TermTracker::new(20, 5, 100);
        t.feed(b"garbage everywhere");
        // ESC[2J clears the screen, ESC[H homes the cursor.
        t.feed(b"\x1b[2J\x1b[Hhi");
        let snap = t.snapshot();
        assert_eq!(snap.text[0].trim_end(), "hi");
        assert_eq!(snap.cursor_row, 0);
        assert_eq!(snap.cursor_col, 2);
    }

    #[test]
    fn invalid_bytes_do_not_wedge_the_tracker() {
        let mut t = TermTracker::new(10, 2, 100);
        // 0xFF is never valid in UTF-8; the tracker must replace-and-continue,
        // not panic or stall, and still render the trailing valid text.
        t.feed(&[b'a', 0xFF, b'b']);
        let text = t.snapshot().text[0].clone();
        assert!(text.starts_with('a'), "got {text:?}");
        assert!(text.contains('b'), "trailing valid byte lost: {text:?}");
    }

    #[test]
    fn resize_changes_reported_dimensions() {
        let mut t = TermTracker::new(80, 24, 100);
        assert_eq!(t.snapshot().cols, 80);
        t.resize(120, 40);
        let snap = t.snapshot();
        assert_eq!(snap.cols, 120);
        assert_eq!(snap.rows, 40);
    }

    /// — regression guard.
    ///
    /// `avt::Buffer::text()` returns the *entire* line history (scrollback +
    /// visible rows + a trailing empty line), **not** just the visible
    /// portion. This surprised us once: a 20×3 terminal fed 10 lines reports
    /// `text().len() == 11`. The browser side of the snapshot apply depends
    /// on this — `agent_claude.html` slices `env.text.slice(-env.rows)` to
    /// recover the viewport-only text before writing to xterm.js. If avt ever
    /// changes `text()` to return only the visible portion, that slice would
    /// silently truncate the visible rows too, and this test would fail.
    ///
    /// Cursor row stays 0-indexed within the *visible* area regardless of
    /// scrollback depth, so a snapshot consumer can pair `text[-rows..]`
    /// directly with `cursor_row` without remapping.
    #[test]
    fn text_includes_scrollback_not_just_visible_rows() {
        let mut t = TermTracker::new(20, 3, 100);
        for i in 0..10 {
            t.feed(format!("line {i}\r\n").as_bytes());
        }
        let snap = t.snapshot();
        // 10 printed lines + 1 trailing empty line, regardless of the
        // 3-row viewport. The visible viewport is the last `rows` entries.
        assert_eq!(
            snap.text.len(),
            11,
            "avt text() should include scrollback; got {} entries",
            snap.text.len()
        );
        // Visible-only slice matches what the browser should paint.
        let visible = &snap.text[snap.text.len() - snap.rows as usize..];
        assert_eq!(visible[0].trim_end(), "line 8");
        assert_eq!(visible[1].trim_end(), "line 9");
        assert_eq!(visible[2].trim_end(), "");
        // Cursor on the last visible row, even though the buffer
        // contains 8 scrollback lines above it.
        // Cursor on the last visible row, even though the buffer
        // contains 8 scrollback lines above it.
        assert_eq!(snap.cursor_row, 2);
        assert_eq!(snap.cursor_col, 0);
    }

    /// Per-physical-row contract: with no wrap, `physical_rows` should
    /// have one entry per visible row, identical in content to the
    /// merged slice. The "one entry per row" invariant is what makes the
    /// browser CSI-positioning safe — `cursor_row` indexes into this Vec
    /// without offsets.
    #[test]
    fn physical_rows_returns_one_entry_per_visible_row() {
        let mut t = TermTracker::new(20, 3, 100);
        for i in 0..4 {
            t.feed(format!("line {i}\r\n").as_bytes());
        }
        let snap = t.snapshot();
        assert_eq!(
            snap.physical_rows.len(),
            snap.rows as usize,
            "physical_rows must equal visible row count"
        );
        // No wrap → each visible row is its own logical line; the merged
        // `text` slice and `physical_rows` should agree on content.
        let visible_text = &snap.text[snap.text.len() - snap.rows as usize..];
        for (i, row) in snap.physical_rows.iter().enumerate() {
            assert_eq!(
                row.trim_end(),
                visible_text[i].trim_end(),
                "row {i} content must match merged text slice"
            );
        }
    }

    /// Contract pin: a `wrapped` row stays its own `physical_rows`
    /// entry, with the wrap continuation visible on the next physical
    /// row. This is the property the browser relies on to place each
    /// row at its own xterm position — the browser does NOT re-merge
    /// rows on apply, so each `physical_rows[i]` must be a single
    /// physical row of <= `cols` chars.
    #[test]
    fn physical_rows_keeps_wrapped_row_as_its_own_entry() {
        // cols=20 forces a 40-char line to wrap exactly once.
        let mut t = TermTracker::new(20, 2, 100);
        t.feed(b"01234567890123456789ABCDEFGHIJKLMNOPQRST");
        let snap = t.snapshot();
        assert_eq!(
            snap.physical_rows.len(),
            2,
            "two visible rows, two physical_rows entries"
        );
        assert_eq!(
            snap.physical_rows[0], "01234567890123456789",
            "first 20 chars on row 0"
        );
        assert_eq!(
            snap.physical_rows[1], "ABCDEFGHIJKLMNOPQRST",
            "continuation on row 1"
        );
        // The merged text, by contrast, collapses the two into one
        // logical-line entry — that's why we ship both.
        assert_eq!(snap.physical_rows[0].len(), 20);
        assert_eq!(snap.physical_rows[1].len(), 20);
    }

    /// Demonstrates the symptom's mechanism: after Claude Code's TUI
    /// issues a clear-EOL on the wrap row, the merged `text()` "fixes"
    /// the merge window (the cleared row's `wrapped` flag flips to
    /// false), absorbing wrap-continuation content into the prior
    /// entry. `physical_rows` keeps the cleared row as its own empty
    /// entry — exactly what the operator's screen looks like on the
    /// wire. The browser cannot recover what the TUI erased, but it
    /// does know where the cursor actually sits.
    #[test]
    fn cleared_wrap_row_becomes_empty_in_physical_rows() {
        let mut t = TermTracker::new(20, 2, 100);
        // Long line wraps to row 1.
        t.feed(b"01234567890123456789ABCDEFGHIJKLMNOPQRST");
        // Cursor at (row 1, col 20). TUI does \x1b[2K to clear the
        // wrap row before painting the next bullet.
        t.feed(b"\x1b[2K");
        let snap = t.snapshot();
        assert_eq!(snap.physical_rows.len(), 2);
        assert_eq!(snap.physical_rows[0], "01234567890123456789");
        assert!(
            snap.physical_rows[1].trim().is_empty(),
            "cleared wrap row is empty/whitespace; got {:?}",
            snap.physical_rows[1]
        );
        // Cursor stays at (row 1, col 20) after the clear — CSI 2K
        // erases cells without moving the cursor. The TUI typically
        // follows up with `\r\n` to position the next bullet.
        assert_eq!(snap.cursor_row, 1);
        assert_eq!(snap.cursor_col, 20);
    }
}
