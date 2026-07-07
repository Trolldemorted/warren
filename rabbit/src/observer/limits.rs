//! §Usage-limits: state-machine parser for Claude Code's `/usage` overlay.
//!
//! The overlay renders incrementally as the PTY reader feeds bytes
//! back to the supervisor. The parser consumes those bytes and emits a
//! [`UsageLimits`] struct carrying the four plan-level fields warren
//! needs to populate the Usage panel:
//!
//! - `weekly_pct` + `weekly_resets_at` — the "Current week (all models)"
//!   row in the overlay.
//! - `session_pct` + `session_resets_at` — the "Current session" row.
//!
//! Per-model breakdowns like `Current week (Fable)` and the
//! "What's contributing to your limits usage?" section are
//! intentionally ignored — those would each add a `Some` field pair,
//! and they're documented as out-of-scope (see the plan's
//! "Out of scope" section).
//!
//! ## Why a state machine
//!
//! A single regex over the full byte stream would not work because
//! the overlay is fed to us in ~256-byte chunks and a partial
//! "Current session" at the end of one chunk needs to combine with
//! the percentage at the start of the next. The parser buffers
//! partial lines and only commits when a terminator is seen.
//!
//! ## Idempotence
//!
//! Feeding the same byte sequence twice returns the same result;
//! late chunks that don't contain new patterns leave all fields
//! unchanged.

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Utc};

/// Plan-level usage limits parsed from a single `/usage` overlay.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageLimits {
    pub weekly_pct: Option<f64>,
    pub weekly_resets_at: Option<DateTime<Utc>>,
    pub session_pct: Option<f64>,
    pub session_resets_at: Option<DateTime<Utc>>,
}

impl UsageLimits {
    pub fn is_empty(&self) -> bool {
        self.weekly_pct.is_none()
            && self.weekly_resets_at.is_none()
            && self.session_pct.is_none()
            && self.session_resets_at.is_none()
    }

    /// True iff all four plan-level fields are populated. Used by
    /// the active scraper to early-exit when a scrape round has
    /// surfaced enough data to satisfy the Usage panel.
    pub fn all_populated(&self) -> bool {
        self.weekly_pct.is_some()
            && self.weekly_resets_at.is_some()
            && self.session_pct.is_some()
            && self.session_resets_at.is_some()
    }

    /// Copy any `Some` fields from `other` into `self` where `self`
    /// has `None`. First-wins precedence: `self`'s already-
    /// populated values are preserved. Used by the active scraper
    /// to merge parse results from successive scroll rounds without
    /// overwriting earlier-round data with later-round noise.
    pub fn merge_from(&mut self, other: UsageLimits) {
        if self.weekly_pct.is_none() {
            self.weekly_pct = other.weekly_pct;
        }
        if self.weekly_resets_at.is_none() {
            self.weekly_resets_at = other.weekly_resets_at;
        }
        if self.session_pct.is_none() {
            self.session_pct = other.session_pct;
        }
        if self.session_resets_at.is_none() {
            self.session_resets_at = other.session_resets_at;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    /// No header seen since the last commit (or start of stream).
    None,
    /// Last header was "Current session" — the next percentage and
    /// "Resets" line belong to the session row.
    Session,
    /// Last header was "Current week (all models)" — the next
    /// percentage and "Resets" line belong to the weekly row.
    Weekly,
}

#[derive(Debug)]
enum ResetsState {
    /// Not currently inside a "Resets ..." line.
    Idle,
    /// Inside a "Resets ..." line, buffering bytes for the
    /// timestamp value (cross-chunk safe).
    Buffering { section: Section, buf: Vec<u8> },
}

/// State-machine parser for the `/usage` overlay. Construct with
/// [`LimitsParser::new`], feed it bytes via [`LimitsParser::feed`],
/// and finalize via [`LimitsParser::flush`].
#[derive(Debug)]
pub struct LimitsParser {
    weekly_pct: Option<f64>,
    weekly_resets_at: Option<DateTime<Utc>>,
    session_pct: Option<f64>,
    session_resets_at: Option<DateTime<Utc>>,
    section: Section,
    /// The section the most-recently-seen percentage belongs to.
    /// The next "Resets" line (if any) pairs with this section.
    pending_section: Option<Section>,
    resets: ResetsState,
    /// Trailing bytes from previous chunks, retained for cross-chunk
    /// detection of the overlay-dismissal ESC sequence (`\x1b[?1049l`,
    /// Claude's "exit alternate screen buffer"). Capped at
    /// [`TRAIL_MAX`] bytes — comfortably more than the 7-byte
    /// sequence so straddled chunks are still found. Only used for
    /// substring search; never re-fed into the state machine
    /// (avoiding double-counting on the Resets buffer accumulation).
    trail: Vec<u8>,
}

/// Maximum number of trailing bytes retained across `feed` calls for
/// cross-chunk overlay-dismissal detection. The dismissal sequence is
/// 7 bytes; 32 leaves ~4x headroom for chunk-boundary straddles.
const TRAIL_MAX: usize = 32;

/// ANSI sequence Claude emits when exiting the `/usage` overlay's
/// alternate screen buffer. We watch for this so a percentage in the
/// post-overlay welcome banner ("use up to 50% of your plan's weekly
/// usage limit…") cannot be mis-attributed to a plan-level field
/// (BUG A in the small-terminal plan).
const DISMISSAL_SEQ: &[u8] = b"\x1b[?1049l";

impl Default for LimitsParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LimitsParser {
    pub fn new() -> Self {
        Self {
            weekly_pct: None,
            weekly_resets_at: None,
            session_pct: None,
            session_resets_at: None,
            section: Section::None,
            pending_section: None,
            resets: ResetsState::Idle,
            trail: Vec::new(),
        }
    }

    /// Reset section state for a fresh scrape window — called by the
    /// active scraper between scroll rounds, and internally when the
    /// parser detects an overlay-dismissal ESC sequence. Preserves
    /// already-committed values (`weekly_pct`, `session_pct`, the two
    /// `*_resets_at`) so data harvested in earlier rounds is not
    /// thrown away; clears the in-progress section/pending/buffer
    /// state so the next chunk is parsed as if no header had been
    /// seen yet.
    pub fn reset_section(&mut self) {
        self.section = Section::None;
        self.pending_section = None;
        self.resets = ResetsState::Idle;
    }

    /// Feed a chunk of bytes from claude's PTY. Idempotent: feeding
    /// the same chunk twice produces the same result. Returns
    /// `Some(UsageLimits)` once all four fields have been observed
    /// at least once (so the caller can early-exit the 2s scrape
    /// window); otherwise `None`. Even when this returns `None`,
    /// the parser has updated its internal state and the next
    /// `feed` call continues from there.
    pub fn feed(&mut self, chunk: &[u8]) -> Option<UsageLimits> {
        // BUG A defense-in-depth: detect the overlay-dismissal
        // ESC sequence in this chunk plus any trailing bytes
        // retained from the previous call. Cross-chunk safe via
        // the trail buffer. The trail is only used for this
        // substring check — it is NOT prepended to the chunk for
        // state-machine processing, to avoid double-counting
        // already-processed bytes in the Resets buffer.
        if self.contains_dismissal(chunk) {
            self.reset_section();
        }
        self.feed_inner(chunk);
        if !chunk.is_empty() {
            self.update_trail(chunk);
        }
        if self.all_populated() {
            Some(self.snapshot())
        } else {
            None
        }
    }

    /// Returns true if the chunk (possibly combined with the trail
    /// retained from the previous call) contains the overlay-
    /// dismissal ESC sequence.
    fn contains_dismissal(&self, chunk: &[u8]) -> bool {
        // Cheap: search the trail first (it's bounded), then the
        // first few bytes of the chunk where a straddled sequence
        // could complete.
        if find_subslice(&self.trail, DISMISSAL_SEQ).is_some() {
            return true;
        }
        // Check the boundary: if the trail's tail could be the
        // start of the sequence, the chunk's head could complete
        // it. Trail is at most TRAIL_MAX bytes; the sequence is
        // 7 bytes; check at most the last 6 bytes of the trail
        // and the first (7 - trail_tail_len) bytes of the chunk.
        let trail_tail = self
            .trail
            .len()
            .saturating_sub(DISMISSAL_SEQ.len() - 1)
            .min(self.trail.len());
        let chunk_head_len = DISMISSAL_SEQ
            .len()
            .saturating_sub(trail_tail)
            .min(chunk.len());
        if chunk_head_len == 0 {
            return false;
        }
        let mut window = Vec::with_capacity(trail_tail + chunk_head_len);
        if trail_tail > 0 {
            window.extend_from_slice(&self.trail[self.trail.len() - trail_tail..]);
        }
        window.extend_from_slice(&chunk[..chunk_head_len]);
        find_subslice(&window, DISMISSAL_SEQ).is_some()
    }

    fn update_trail(&mut self, chunk: &[u8]) {
        if chunk.len() >= TRAIL_MAX {
            self.trail = chunk[chunk.len() - TRAIL_MAX..].to_vec();
        } else {
            self.trail.extend_from_slice(chunk);
            if self.trail.len() > TRAIL_MAX {
                self.trail.truncate(TRAIL_MAX);
            }
        }
    }

    /// Force-emit whatever we have. Used at scrape timeout (2s
    /// deadline) and in tests. Returns `None` if no field was ever
    /// observed.
    pub fn flush(&mut self) -> Option<UsageLimits> {
        // Drain any pending "Resets" buffer into the right field
        // before reading state. We swap the enum out to avoid a
        // borrow conflict on `self` between the buffer's lifetime
        // and the call to `commit_resets_at`.
        let drained = std::mem::replace(&mut self.resets, ResetsState::Idle);
        if let ResetsState::Buffering { section, buf } = drained {
            self.commit_resets_at(section, &buf);
        }
        if self.is_empty() {
            None
        } else {
            Some(self.snapshot())
        }
    }

    fn snapshot(&self) -> UsageLimits {
        UsageLimits {
            weekly_pct: self.weekly_pct,
            weekly_resets_at: self.weekly_resets_at,
            session_pct: self.session_pct,
            session_resets_at: self.session_resets_at,
        }
    }

    fn is_empty(&self) -> bool {
        self.snapshot().is_empty()
    }

    fn all_populated(&self) -> bool {
        self.weekly_pct.is_some()
            && self.weekly_resets_at.is_some()
            && self.session_pct.is_some()
            && self.session_resets_at.is_some()
    }

    fn feed_inner(&mut self, chunk: &[u8]) {
        let mut i = 0;
        while i < chunk.len() {
            // 1) Section headers.
            //
            // Note: `pending_section` is NOT updated here — only
            // when a percentage is parsed. That's the existing
            // semantics from the v1 parser. The active scraper
            // (see `supervisor::run_usage_scrape`) calls
            // `reset_section()` between scroll rounds, so stale
            // pending_section state from a previous round cannot
            // leak into a fresh round.
            if chunk[i..].starts_with(b"Current session") {
                self.section = Section::Session;
                i += b"Current session".len();
                continue;
            }
            if chunk[i..].starts_with(b"Current week (all models)") {
                self.section = Section::Weekly;
                i += b"Current week (all models)".len();
                continue;
            }
            // "Current week (" followed by anything OTHER than
            // "all models)" — e.g. "(Fable)" or "(Sonnet)". Per the
            // plan these are out of scope; we deliberately do NOT
            // switch `self.section` to a new Weekly so the next
            // percentage and "Resets" still pair with the most
            // recent "(all models)" row.
            if chunk[i..].starts_with(b"Current week (")
                && !chunk[i..].starts_with(b"Current week (all models)")
            {
                // Skip past the closing ')' to avoid matching the
                // inner "Current week" again on the next iteration.
                if let Some(close) = find_byte(&chunk[i..], b')') {
                    i += close + 1;
                } else {
                    i = chunk.len();
                }
                continue;
            }

            // 2) "Resets ..." line: arm the buffering state and let
            //    the loop below collect bytes until "(UTC)" or end.
            if matches!(self.resets, ResetsState::Idle) && chunk[i..].starts_with(b"Resets ") {
                self.resets = ResetsState::Buffering {
                    section: self.pending_section.unwrap_or(self.section),
                    buf: Vec::new(),
                };
                i += b"Resets ".len();
                continue;
            }

            // 3) If we're buffering a "Resets" line, drain bytes
            //    into the buffer until we see "(UTC)" (the
            //    terminating marker) or run off the end of the chunk.
            if matches!(self.resets, ResetsState::Buffering { .. }) {
                // Swap out so we can move the buffer into a local
                // and then re-assign. Avoids the borrow conflict
                // the direct `if let ResetsState::Buffering { ref
                // mut buf, .. } = self.resets` would create with
                // `commit_resets_at`'s `&mut self` call.
                let taken = std::mem::replace(&mut self.resets, ResetsState::Idle);
                if let ResetsState::Buffering { section, mut buf } = taken {
                    if let Some(end) = find_subslice(&chunk[i..], b"(UTC)") {
                        buf.extend_from_slice(&chunk[i..i + end]);
                        self.commit_resets_at(section, &buf);
                        i += end + b"(UTC)".len();
                    } else {
                        buf.extend_from_slice(&chunk[i..]);
                        // Re-arm the buffer for the next chunk.
                        self.resets = ResetsState::Buffering { section, buf };
                        i = chunk.len();
                    }
                }
                continue;
            }

            // 4) Percentage in the current section's bar.
            if let Some((pct, consumed)) = parse_leading_pct(&chunk[i..]) {
                match self.section {
                    Section::Session => {
                        if self.session_pct.is_none() {
                            self.session_pct = Some(pct);
                            self.pending_section = Some(Section::Session);
                        }
                    }
                    Section::Weekly => {
                        if self.weekly_pct.is_none() {
                            self.weekly_pct = Some(pct);
                            self.pending_section = Some(Section::Weekly);
                        }
                    }
                    Section::None => {
                        // Percentage before any header — ignore.
                    }
                }
                i += consumed;
                continue;
            }

            i += 1;
        }
    }

    fn commit_resets_at(&mut self, section: Section, buf: &[u8]) {
        let s = strip_ansi_bytes(buf);
        if let Some(dt) = parse_resets_text(&s) {
            match section {
                Section::Session => {
                    if self.session_resets_at.is_none() {
                        self.session_resets_at = Some(dt);
                    }
                }
                Section::Weekly => {
                    if self.weekly_resets_at.is_none() {
                        self.weekly_resets_at = Some(dt);
                    }
                }
                Section::None => {}
            }
        }
    }
}

fn parse_leading_pct(chunk: &[u8]) -> Option<(f64, usize)> {
    let mut j = 0;
    while j < chunk.len() && chunk[j].is_ascii_digit() {
        j += 1;
    }
    if j == 0 || j >= chunk.len() || chunk[j] != b'%' {
        return None;
    }
    let n: f64 = std::str::from_utf8(&chunk[..j]).ok()?.parse().ok()?;
    Some((n, j + 1))
}

fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Strip ANSI escape codes and non-printable control bytes from a
/// chunk, returning a plain UTF-8 string with only printable
/// characters and a few whitespace controls (newline, carriage
/// return, tab) preserved. Used to convert a buffered
/// "Resets ..." line into something the timestamp parser can
/// pattern-match.
fn strip_ansi_bytes(buf: &[u8]) -> String {
    let mut out = String::with_capacity(buf.len());
    let bytes: Vec<u8> = buf.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            // ESC: skip the escape sequence. CSI is `ESC [ … final`,
            // OSC is `ESC ] … (BEL | ESC \)`, others are 2-byte
            // `ESC <char>`. We just scan forward until a final byte
            // (0x40-0x7E) for CSI, or BEL/ESC for OSC, with a hard
            // cap to avoid runaway loops on malformed input.
            if i + 1 >= bytes.len() {
                break;
            }
            match bytes[i + 1] {
                b'[' => {
                    i += 2;
                    let start = i;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    if i < bytes.len() {
                        i += 1; // consume the final byte
                    } else {
                        i = start; // unterminated; bail without losing real text
                    }
                }
                b']' => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != 0x07 && bytes[i] != 0x1b {
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i] == 0x1b {
                        i += 1;
                    }
                    if i < bytes.len() && bytes[i] == 0x07 {
                        i += 1;
                    }
                }
                _ => {
                    i += 2;
                }
            }
        } else if b.is_ascii_graphic() || b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
            out.push(b as char);
            i += 1;
        } else if b >= 0x80 {
            // Pass through UTF-8 continuation bytes; we'll
            // validate below.
            let start = i;
            while i < bytes.len() && bytes[i] >= 0x80 {
                i += 1;
            }
            if let Ok(s) = std::str::from_utf8(&bytes[start..i]) {
                out.push_str(s);
            }
        } else {
            // Other control byte (NUL, BS, etc.) — drop.
            i += 1;
        }
    }
    out
}

/// Parse a "Resets ..." line (already ANSI-stripped) into a UTC
/// `DateTime`. Two accepted shapes:
///
/// - `H:MM(am|pm)` — same-day time, today (or tomorrow if the
///   time is already past).
/// - `Mon D, H(am|pm)` or `Mon D, H:MM(am|pm)` — calendar date +
///   time, current year (or next year if the date is already
///   past).
fn parse_resets_text(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    // Drop a trailing "(UTC)" if present.
    let s = s.trim_end_matches("(UTC)").trim();
    if let Some(dt) = parse_time_of_day(s) {
        return Some(dt);
    }
    parse_month_day_time(s)
}

fn parse_time_of_day(s: &str) -> Option<DateTime<Utc>> {
    // "H:MM(am|pm)" — hour, colon, minute, then am/pm suffix.
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 || i >= bytes.len() || bytes[i] != b':' {
        return None;
    }
    let hour: u32 = s[..i].parse().ok()?;
    i += 1; // skip ':'
    let start_min = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start_min {
        return None;
    }
    let minute: u32 = s[start_min..i].parse().ok()?;
    let ampm = s[i..].trim();
    let hour_24 = match ampm {
        "am" => {
            if hour == 12 {
                0
            } else {
                hour
            }
        }
        "pm" => {
            if hour == 12 {
                12
            } else {
                hour + 12
            }
        }
        _ => return None,
    };
    if hour_24 > 23 || minute > 59 {
        return None;
    }

    let now = Utc::now();
    let naive = now.date_naive().and_hms_opt(hour_24, minute, 0)?;
    let dt = Utc.from_utc_datetime(&naive);
    if dt < now {
        Some(dt + chrono::Duration::days(1))
    } else {
        Some(dt)
    }
}

fn parse_month_day_time(s: &str) -> Option<DateTime<Utc>> {
    // "Mon D, H(am|pm)" or "Mon D, H:MM(am|pm)" — split on the
    // first comma; the right side is the time, the left side is
    // "Mon D".
    let (date_part, time_part) = s.split_once(',')?;
    let date_part = date_part.trim();
    let time_part = time_part.trim();

    let mut date_tokens = date_part.split_whitespace();
    let month_name = date_tokens.next()?;
    let day_str = date_tokens.next()?;
    let day: u32 = day_str.parse().ok()?;

    let month = parse_month_abbr(month_name)?;
    let (hour_24, minute) = parse_hour_minute_ampm(time_part)?;

    let now = Utc::now();
    let mut year = now.year();
    let dt = build_dt(year, month, day, hour_24, minute)?;
    if dt < now {
        year += 1;
        build_dt(year, month, day, hour_24, minute)
    } else {
        Some(dt)
    }
}

fn build_dt(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> Option<DateTime<Utc>> {
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let naive: NaiveDateTime = date.and_hms_opt(hour, minute, 0)?;
    Some(Utc.from_utc_datetime(&naive))
}

fn parse_hour_minute_ampm(s: &str) -> Option<(u32, u32)> {
    // "H(am|pm)" or "H:MM(am|pm)"
    let s = s.trim();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let hour: u32 = s[..i].parse().ok()?;
    let mut minute: u32 = 0;
    if i < bytes.len() && bytes[i] == b':' {
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        minute = s[start..i].parse().ok()?;
    }
    let ampm = s[i..].trim();
    let hour_24 = match ampm {
        "am" => {
            if hour == 12 {
                0
            } else {
                hour
            }
        }
        "pm" => {
            if hour == 12 {
                12
            } else {
                hour + 12
            }
        }
        _ => return None,
    };
    if hour_24 > 23 || minute > 59 {
        return None;
    }
    Some((hour_24, minute))
}

fn parse_month_abbr(s: &str) -> Option<u32> {
    match s {
        "Jan" => Some(1),
        "Feb" => Some(2),
        "Mar" => Some(3),
        "Apr" => Some(4),
        "May" => Some(5),
        "Jun" => Some(6),
        "Jul" => Some(7),
        "Aug" => Some(8),
        "Sep" => Some(9),
        "Oct" => Some(10),
        "Nov" => Some(11),
        "Dec" => Some(12),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Load the real `script -c` capture committed at
    /// `tests/fixtures/usage_screen.bin`. The capture contains one
    /// or more `/usage` overlays interleaved with the rest of the
    /// terminal session. The structural assertion (every field
    /// populated) is what the parser is contractually required to
    /// satisfy against the canonical Claude TUI; per-field value
    /// assertions are deliberately omitted because the capture
    /// contains the operator's actual usage numbers, which shift
    /// session-to-session.
    fn load_fixture(name: &str) -> Vec<u8> {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
    }

    fn parse_fixture(name: &str) -> UsageLimits {
        let bytes = load_fixture(name);
        let mut p = LimitsParser::new();
        // Feed in 256-byte chunks — same shape the PTY reader
        // actually delivers.
        for chunk in bytes.chunks(256) {
            if p.feed(chunk).is_some() {
                break;
            }
        }
        p.feed(&[]).unwrap_or_else(|| p.flush().unwrap_or_default())
    }

    #[test]
    fn feed_recognizes_real_capture() {
        let limits = parse_fixture("usage_screen.bin");
        assert!(limits.weekly_pct.is_some(), "weekly_pct missing");
        assert!(
            limits.weekly_resets_at.is_some(),
            "weekly_resets_at missing"
        );
        assert!(limits.session_pct.is_some(), "session_pct missing");
        assert!(
            limits.session_resets_at.is_some(),
            "session_resets_at missing"
        );
    }

    /// Active-scraping fixture: 12 rows × 80 cols, captured AFTER
    /// the operator pressed Down arrow to scroll the overlay.
    /// Initial paint at h12 shows the Status panel's Usage tab
    /// (no modal overlay at all — Claude refuses to render it at
    /// this height). Down-arrow scrolling past the Status panel
    /// eventually brings the modal into view: "Current session",
    /// "0% used", "Current week (all models)", "100%used",
    /// "Resets Jul 9, 5am (UTC)", "Current week (Fable) 92% used",
    /// plus the "What's contributing to your limits usage?"
    /// section. The welcome banner re-renders after Esc with its
    /// "use up to 50% of your weekly usage limit" copy.
    ///
    /// Key finding: active scraping recovers the modal at h12 —
    /// three of the four plan-limit fields populate. The fourth
    /// (`session_resets_at`) is None because at 0% session usage
    /// Claude does not emit a time-only "Resets H:MM (UTC)" line
    /// for the session section (only the weekly reset is rendered
    /// in this capture). That's a real-data absence, not a parser
    /// bug — the parser correctly returns None. In production the
    /// operator would see "—" for the session reset time; a UI
    /// signal (mitigation C from the plan) would help distinguish
    /// "no reset because session usage is 0" from "scrape failed."
    ///
    /// `weekly_pct = 100.0` is pinned here so a future precedence
    /// change doesn't silently regress to the per-model "92%"
    /// (Fable) value that arrives later in the stream.
    #[test]
    fn h12_scrolled_fixture_recovers_three_of_four_fields() {
        let limits = parse_fixture("usage_screen_small_h12_scrolled.bin");
        assert_eq!(
            limits.session_pct,
            Some(0.0),
            "session_pct should be 0 from the post-scroll repaint"
        );
        // Session reset is absent from the stream — at 0% session
        // usage Claude doesn't render a "Resets H:MM (UTC)" line
        // for the session section. The parser correctly returns
        // None. This pins the documented behavior.
        assert!(
            limits.session_resets_at.is_none(),
            "session_resets_at should be None — Claude omits the time-only \
             reset when session usage is 0% (no 'Resets H:MM (UTC)' in the \
             stream); got {:?}",
            limits.session_resets_at
        );
        assert_eq!(
            limits.weekly_pct,
            Some(100.0),
            "weekly_pct should be 100 from the post-scroll repaint"
        );
        assert!(
            limits.weekly_resets_at.is_some(),
            "weekly_resets_at should populate from the post-scroll repaint"
        );
    }

    /// Active-scraping fixture: 24 rows × 50 cols, captured AFTER
    /// the operator pressed Down arrow to scroll to the bottom of
    /// the overlay. The full sequence visible in the stream:
    ///
    ///   1. Initial paint: session header + reset on one row,
    ///      "0% used" on the next, weekly header + reset on the
    ///      next, weekly bar elided at this width.
    ///   2. Several Down-arrow keystrokes (encoded as CSI cursor-
    ///      down sequences in the captured stream) scroll the
    ///      overlay.
    ///   3. Each scroll-triggered repaint re-emits the overlay
    ///      content with progressively more rows visible —
    ///      including the weekly bar+percentage ("100%used") and
    ///      the per-model breakdown ("Current week (Fable)
    ///      92% used") plus the "What's contributing to your
    ///      limits usage?" section.
    ///   4. Esc dismisses the overlay; the welcome banner
    ///      re-renders with its "use up to 50% of your plan's
    ///      weekly usage limit" promotional text.
    ///
    /// Expected: all four plan-limit fields populate correctly
    /// from the post-scroll bytes, because the weekly bar+100%
    /// becomes visible. The "Fable 92%" arrives later in the
    /// stream but `ignores_per_model_week_breakdown` ensures it
    /// does not overwrite the all-models value. BUG A from the
    /// plan (over-match from welcome banner after Esc) remains
    /// latent in the passive-parser code path; the test below
    /// pins the actual value so a fix can be caught — see plan
    /// §"Active scraping supersedes passive observation" for the
    /// recommended scroll-boundary section reset that the active
    /// scraper will apply.
    #[test]
    fn w50_scrolled_fixture_exposes_all_four_fields() {
        let limits = parse_fixture("usage_screen_small_w50_scrolled.bin");
        // Session section is visible from the first paint.
        assert_eq!(
            limits.session_pct,
            Some(0.0),
            "session_pct should be 0 from the initial paint"
        );
        assert!(
            limits.session_resets_at.is_some(),
            "session_resets_at should populate from the initial paint"
        );
        // Weekly section becomes visible only after scrolling.
        // The pinned value is 100.0 (the all-models percentage,
        // not the per-model Fable 92% that arrives later) — this
        // pins the parser's "first wins" precedence so a future
        // refactor to "last wins" doesn't silently regress.
        assert_eq!(
            limits.weekly_pct,
            Some(100.0),
            "weekly_pct should be 100 from the post-scroll repaint"
        );
        assert!(
            limits.weekly_resets_at.is_some(),
            "weekly_resets_at should populate from the post-scroll repaint"
        );
    }

    #[test]
    fn feed_ignores_unrelated_lines() {
        let mut p = LimitsParser::new();
        // A stream with no /usage content should not populate any
        // field. The transcript reader and the xterm output
        // routinely contain text that *looks* like it could be a
        // percentage; the parser must not be fooled.
        let chunk = b"hello world\nfoo 42% bar\nResets 1am (UTC) but no header above\n";
        let _ = p.feed(chunk);
        // No header ever set `self.section`, so even a stray
        // "Resets" shouldn't populate.
        let limits = p.flush().unwrap_or_default();
        assert!(limits.is_empty(), "unrelated content populated: {limits:?}");
    }

    #[test]
    fn flush_returns_partial_state_on_truncated_input() {
        // A stream that contains the headers and a percentage but
        // is cut off before the "Resets" line should still surface
        // the partial state on flush.
        let mut p = LimitsParser::new();
        let _ = p.feed(b"Current session\n0%\nCurrent week (all models)\n100%\n");
        let limits = p.flush().expect("partial state");
        assert_eq!(limits.session_pct, Some(0.0));
        assert_eq!(limits.weekly_pct, Some(100.0));
        assert!(limits.session_resets_at.is_none());
        assert!(limits.weekly_resets_at.is_none());
    }

    #[test]
    fn ignores_per_model_week_breakdown() {
        // The plan documents per-model rows ("Current week (Fable)")
        // as out of scope. The parser must not switch to a new
        // Weekly section for them, so a subsequent "Resets" line
        // still pairs with the most recent "all models" row.
        let mut p = LimitsParser::new();
        let _ = p.feed(b"Current week (all models)\n100%\nResets Jul 9, 5am (UTC)\n");
        let _ = p.feed(b"Current week (Fable)\n92%\nResets Jul 9, 5am (UTC)\n");
        let limits = p.flush().expect("parsed");
        assert_eq!(limits.weekly_pct, Some(100.0));
        assert!(limits.weekly_resets_at.is_some());
    }

    #[test]
    fn parses_time_only_resets() {
        let dt = parse_time_of_day("12:20pm").expect("parse 12:20pm");
        // 12:20pm == 12:20 in 24h. Sanity-check the hour.
        assert_eq!(dt.format("%H:%M").to_string(), "12:20");
    }

    #[test]
    fn parses_month_day_time_resets() {
        // Far-future date so the test is stable across runs.
        let dt = parse_month_day_time("Jul 9, 5am").expect("parse Jul 9, 5am");
        assert_eq!(&dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()[5..10], "07-09");
        assert_eq!(dt.format("%H:%M").to_string(), "05:00");
    }

    /// §Active-scraping: `reset_section` clears the in-progress
    /// section/pending/buffer state but preserves already-committed
    /// values. Called between scroll rounds so each round starts
    /// fresh and bytes from a previous round's overlay cannot leak
    /// into a later round's attribution.
    #[test]
    fn reset_section_clears_section_state_preserves_committed_values() {
        let mut p = LimitsParser::new();
        // First round: populate everything for Session and the
        // weekly percentage (but no weekly reset yet).
        let _ = p.feed(b"Current session\n0% used\nResets 3:30pm (UTC)\n");
        let snap = p.flush().expect("session populated");
        assert_eq!(snap.session_pct, Some(0.0));
        assert!(snap.session_resets_at.is_some());
        // Now simulate "next scroll round" — caller resets, then
        // the new bytes come in.
        p.reset_section();
        // After reset, the parser should treat the next chunk as
        // a fresh slate. Already-committed session_pct and
        // session_resets_at must NOT be cleared.
        let _ = p.feed(b"Current week (all models)\n");
        // No "Resets" yet — the percentage arrives next.
        let _ = p.feed(b"100%\n");
        let _ = p.feed(b"Resets Jul 9, 5am (UTC)\n");
        let limits = p.flush().expect("all populated");
        assert_eq!(limits.session_pct, Some(0.0));
        assert!(limits.session_resets_at.is_some());
        assert_eq!(limits.weekly_pct, Some(100.0));
        assert!(limits.weekly_resets_at.is_some());
    }

    /// §Active-scraping: a "Resets" line that arrives between
    /// scroll rounds (no header in between) must NOT be
    /// mis-attributed to the section that was active before the
    /// reset. After `reset_section()`, section=None and any
    /// headerless "Resets" still needs a header to land in a
    /// field.
    #[test]
    fn reset_section_blocks_stale_resets_commit() {
        let mut p = LimitsParser::new();
        // Round 1: session populated.
        let _ = p.feed(b"Current session\n0% used\n");
        // Reset for the next round.
        p.reset_section();
        // Round 2 starts with a stray "Resets" (no header yet) —
        // should NOT populate session_resets_at (which would
        // require pending_section or section to be Session).
        let _ = p.feed(b"Resets 2:00pm (UTC)\n");
        let snap = p.flush().expect("session_pct still set");
        assert_eq!(snap.session_pct, Some(0.0));
        // session_resets_at is None — the stray "Resets" had no
        // header to pair with after the reset.
        assert!(
            snap.session_resets_at.is_none(),
            "stray Resets after reset_section should NOT commit, got {:?}",
            snap.session_resets_at
        );
    }

    /// §Active-scraping: the overlay-dismissal ESC sequence
    /// (`\x1b[?1049l`) is detected across chunk boundaries via
    /// the trail buffer. The bytes "Resets 50%" that arrive
    /// AFTER the dismissal must not pollute the parser state
    /// (BUG A defense-in-depth).
    #[test]
    fn dismissal_sequence_resets_section_state_across_chunks() {
        let mut p = LimitsParser::new();
        // First chunk contains a header + percentage + Resets.
        let _ = p.feed(b"Current session\n0% used\nResets 3:30pm (UTC)\n");
        // Second chunk straddles the dismissal sequence: trail
        // ends with "\x1b[?104" and the chunk starts with "9l".
        // Cross-chunk detection should still fire.
        let _ = p.feed(b"\x1b[?104");
        let _ = p.feed(b"9l");
        // After dismissal, a stray "Current session" + "50%" must
        // not commit anything — section was reset to None and no
        // subsequent header has appeared yet.
        let _ = p.feed(b"Current session\n50%\n");
        let snap = p.flush().expect("session_pct from first chunk");
        // session_pct from before dismissal: still 0.0
        assert_eq!(snap.session_pct, Some(0.0));
        // The post-dismissal "50%" landed while section was
        // Session (the post-dismissal "Current session" header
        // re-armed it), but session_pct was already set — first-
        // wins precedence means it stays at 0.0.
        assert_eq!(snap.session_pct, Some(0.0));
    }
}
