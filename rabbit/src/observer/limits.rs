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
}

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
        }
    }

    /// Feed a chunk of bytes from claude's PTY. Idempotent: feeding
    /// the same chunk twice produces the same result. Returns
    /// `Some(UsageLimits)` once all four fields have been observed
    /// at least once (so the caller can early-exit the 2s scrape
    /// window); otherwise `None`. Even when this returns `None`,
    /// the parser has updated its internal state and the next
    /// `feed` call continues from there.
    pub fn feed(&mut self, chunk: &[u8]) -> Option<UsageLimits> {
        self.feed_inner(chunk);
        if self.all_populated() {
            Some(self.snapshot())
        } else {
            None
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
    fn load_fixture() -> Vec<u8> {
        let path = format!(
            "{}/tests/fixtures/usage_screen.bin",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
    }

    #[test]
    fn feed_recognizes_real_capture() {
        let bytes = load_fixture();
        let mut p = LimitsParser::new();
        // Feed in 256-byte chunks — same shape the PTY reader
        // actually delivers.
        for chunk in bytes.chunks(256) {
            if p.feed(chunk).is_some() {
                break;
            }
        }
        let limits = p
            .feed(&[])
            .unwrap_or_else(|| p.flush().expect("parser should have seen /usage"));
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
}
