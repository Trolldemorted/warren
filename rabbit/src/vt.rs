//! §D Milestone 5 — server-side virtual terminal state (Phase A).
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
    pub text: Vec<String>,
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
        ScreenSnapshot {
            cols: cols as u16,
            rows: rows as u16,
            cursor_col: cursor.col as u16,
            cursor_row: cursor.row as u16,
            cursor_visible: cursor.visible,
            text: self.vt.text(),
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
}
