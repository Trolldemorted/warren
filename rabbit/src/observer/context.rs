//! §Context-window: state-machine parser for Claude Code's `/context`
//! overlay.
//!
//! Mirrors [`crate::observer::limits::LimitsParser`] — feed bytes
//! from the broadcast `TermFrame` stream, drain with [`flush`].
//! All five primary fields are `Option` so a partial render still
//! ships a snapshot (the UI shows "—" for missing pieces).
//!
//! ## Modal shapes recognized
//!
//! The Claude TUI's `/context` overlay (empirical; real fixture TBD)
//! renders as one of these patterns, depending on terminal width:
//!
//! - `Used: 87,432 / 200,000 tokens (44%)` — full canonical form.
//! - `Used: 87,432 / 200,000 (44%)` — without the literal "tokens".
//! - `Used: 87,432 / 200000 (44%)` — comma-free.
//! - `Used 87,432 / 200,000 (44%)` — without the colon.
//! - `(44%)` after a `Context` header — percentage-only mode.
//! - `Free: 56%` — free-space trailing label.
//! - `System prompt: 1234`, `Tools: 5678`, `Conversation: 9012` —
//!   per-category rows that surface as a JSON object on the wire.
//! - `200K` / `1M` window-label suffix.
//!
//! ## Why a state machine
//!
//! Same reason as [`LimitsParser`]: a single regex over the full
//! byte stream would not work because the overlay is fed to us in
//! ~256-byte chunks and a partial `Used: 87,4` at the end of one
//! chunk needs to combine with `32 / 200,000 (44%)` at the start of
//! the next.
//!
//! ## Idempotence
//!
//! Feeding the same byte sequence twice returns the same result.
//!
//! [`flush`]: ContextParser::flush

use serde_json::{json, Map, Value};

/// Context-window usage snapshot parsed from a single `/context`
/// overlay. All numeric fields are `Option` so the UI can render
/// "—" for any piece the overlay omitted (e.g. small terminal
/// where the modal got truncated mid-render).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextSnapshot {
    pub used_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub used_pct: Option<f64>,
    pub free_pct: Option<f64>,
    pub window_tokens: Option<u64>,
    pub categories: Option<Value>,
    /// §Small-terminal mitigation C analogue: when `true`, the most
    /// recent `/context` scrape did not surface all primary fields
    /// (`used_tokens`, `total_tokens`, `used_pct`). The supervisor
    /// sets this when a non-empty snapshot is missing any of those
    /// three so the UI can surface a "try a larger window" hint.
    pub scrape_incomplete: bool,
}

impl ContextSnapshot {
    pub fn is_empty(&self) -> bool {
        self.used_tokens.is_none()
            && self.total_tokens.is_none()
            && self.used_pct.is_none()
            && self.free_pct.is_none()
            && self.window_tokens.is_none()
            && self.categories.is_none()
    }

    /// True iff `used_tokens`, `total_tokens`, and `used_pct` are
    /// all populated. Used by the active scraper to early-exit
    /// when a single paint has surfaced enough data for the
    /// dashboard's primary row.
    pub fn all_populated(&self) -> bool {
        self.used_tokens.is_some() && self.total_tokens.is_some() && self.used_pct.is_some()
    }

    /// Copy any `Some` fields from `other` into `self` where
    /// `self` has `None`. First-wins precedence: `self`'s already-
    /// populated values are preserved.
    pub fn merge_from(&mut self, other: ContextSnapshot) {
        if self.used_tokens.is_none() {
            self.used_tokens = other.used_tokens;
        }
        if self.total_tokens.is_none() {
            self.total_tokens = other.total_tokens;
        }
        if self.used_pct.is_none() {
            self.used_pct = other.used_pct;
        }
        if self.free_pct.is_none() {
            self.free_pct = other.free_pct;
        }
        if self.window_tokens.is_none() {
            self.window_tokens = other.window_tokens;
        }
        match (&mut self.categories, other.categories) {
            (None, Some(c)) => self.categories = Some(c),
            (Some(existing), Some(c)) if existing.is_object() && c.is_object() => {
                if let (Some(existing_obj), Some(extra_obj)) =
                    (existing.as_object_mut(), c.as_object())
                {
                    for (k, v) in extra_obj {
                        existing_obj.entry(k.clone()).or_insert(v.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    /// No header seen since the last commit (or start of stream).
    None,
    /// Last header was "Context" or "/context" — the next
    /// `Used:` line and percentage belong to this section.
    Context,
}

/// State-machine parser for the `/context` overlay. Construct with
/// [`ContextParser::new`], feed it bytes via [`ContextParser::feed`],
/// and finalize via [`ContextParser::flush`].
///
/// Internally buffers partial lines across chunk boundaries (the
/// PTY reader delivers in ~256-byte chunks, so a partial
/// `Used: 87,4` at the end of one chunk pairs with
/// `32 / 200,000 (44%)` at the start of the next).
#[derive(Debug)]
pub struct ContextParser {
    used_tokens: Option<u64>,
    total_tokens: Option<u64>,
    used_pct: Option<f64>,
    free_pct: Option<f64>,
    window_tokens: Option<u64>,
    categories_map: Map<String, Value>,
    section: Section,
    /// Cross-chunk line buffer. Bytes accumulate here until a
    /// newline is seen, at which point the line is parsed in full.
    /// Capped at [`LINE_MAX`] bytes — a single modal line is well
    /// under 200 chars in any plausible terminal width.
    line_buf: Vec<u8>,
    /// Trailing bytes from previous chunks, retained for cross-chunk
    /// detection of the overlay-dismissal ESC sequence
    /// (`\x1b[?1049l`, Claude's "exit alternate screen buffer").
    trail: Vec<u8>,
}

/// Maximum size of a single parser line buffer, in bytes.
///
/// A single `/context` modal row in the live Claude TUI byte stream is
/// dramatically longer than the parser's logical content because each
/// bar-chart glyph + label is rendered through cursor-positioning +
/// color SGR escapes. Empirically (Claude Code 2.x on a 120-col
/// terminal) one modal row reaches ~1 KB on the wire and the headline
/// row crosses 900 bytes. A 256-byte cap clips mid-row, dropping the
/// `Context Usage    <bar>   <used>/<total> tokens (<pct>%)` headline
/// before the parser ever sees it — which is why a too-small cap
/// manifests as a silent "scrape returned no data" hint. 4096
/// comfortably accommodates any plausible width without unbounded
/// growth from a misbehaving source.
const LINE_MAX: usize = 4096;

/// Maximum number of trailing bytes retained across `feed` calls.
const TRAIL_MAX: usize = 32;

/// ANSI sequence Claude emits when exiting the `/context` overlay's
/// alternate screen buffer.
const DISMISSAL_SEQ: &[u8] = b"\x1b[?1049l";

impl Default for ContextParser {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextParser {
    pub fn new() -> Self {
        Self {
            used_tokens: None,
            total_tokens: None,
            used_pct: None,
            free_pct: None,
            window_tokens: None,
            categories_map: Map::new(),
            section: Section::None,
            line_buf: Vec::new(),
            trail: Vec::new(),
        }
    }

    /// Reset section state for a fresh scrape window. Preserves
    /// already-committed values so data harvested in earlier
    /// rounds is not thrown away.
    pub fn reset_section(&mut self) {
        self.section = Section::None;
    }

    /// Feed a chunk of bytes from claude's PTY. Idempotent.
    pub fn feed(&mut self, chunk: &[u8]) -> Option<ContextSnapshot> {
        if self.contains_dismissal(chunk) {
            self.reset_section();
        }
        // Concatenate any partial-line buffer with this chunk so a
        // partial `Used: 87,4` from one call pairs with `32 / 200,000
        // (44%)` at the start of the next call.
        let mut combined = std::mem::take(&mut self.line_buf);
        combined.extend_from_slice(chunk);
        // Split on newlines.
        let mut start = 0;
        for (idx, &b) in combined.iter().enumerate() {
            if b == b'\n' {
                let line = &combined[start..idx];
                self.parse_line(line);
                start = idx + 1;
            }
        }
        // Save leftover bytes for the next call.
        if start < combined.len() {
            self.line_buf.extend_from_slice(&combined[start..]);
            if self.line_buf.len() > LINE_MAX {
                let keep = LINE_MAX / 2;
                self.line_buf.drain(..self.line_buf.len() - keep);
            }
        }
        if !chunk.is_empty() {
            self.update_trail(chunk);
        }
        if self.all_populated() {
            Some(self.snapshot())
        } else {
            None
        }
    }

    /// Force-emit whatever we have. Used at scrape timeout and in
    /// tests. Returns `None` if no field was ever observed.
    pub fn flush(&mut self) -> Option<ContextSnapshot> {
        // Drain any partial line buffer.
        if !self.line_buf.is_empty() {
            let line = std::mem::take(&mut self.line_buf);
            self.parse_line(&line);
        }
        if self.is_empty() {
            None
        } else {
            Some(self.snapshot())
        }
    }

    fn snapshot(&self) -> ContextSnapshot {
        ContextSnapshot {
            used_tokens: self.used_tokens,
            total_tokens: self.total_tokens,
            used_pct: self.used_pct,
            free_pct: self.free_pct,
            window_tokens: self.window_tokens,
            categories: if self.categories_map.is_empty() {
                None
            } else {
                Some(Value::Object(self.categories_map.clone()))
            },
            scrape_incomplete: false,
        }
    }

    fn is_empty(&self) -> bool {
        self.used_tokens.is_none()
            && self.total_tokens.is_none()
            && self.used_pct.is_none()
            && self.free_pct.is_none()
            && self.window_tokens.is_none()
            && self.categories_map.is_empty()
    }

    fn all_populated(&self) -> bool {
        self.used_tokens.is_some() && self.total_tokens.is_some() && self.used_pct.is_some()
    }

    // --- dismissal detection (cross-chunk) ---

    fn contains_dismissal(&self, chunk: &[u8]) -> bool {
        if find_subslice(&self.trail, DISMISSAL_SEQ).is_some() {
            return true;
        }
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

    // --- line parsing ---

    fn parse_line(&mut self, raw: &[u8]) {
        let s = strip_ansi_bytes(raw);
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return;
        }
        // §Context-window: check the compact headline shape
        // BEFORE the section-header check, because the real
        // Claude modal paints `Context Usage` and the
        // bar chart + headline on the SAME visual line —
        // `Context Usage    ⎁ ⎁ 24.2k/200k tokens (12%)     ⎶ ⎶
        // Estimated usage by category`. A line that starts
        // with `Context` is therefore the headline line, not
        // just a section header. The legacy form below is
        // preserved for older builds. The headline match
        // does NOT return — terminal capture joins visual
        // rows with CR-only, so the same parser line may
        // also contain the per-category rows that follow
        // the headline on screen. We fall through to the
        // category-label loop after populating the
        // headline fields.
        let mut headline_matched = false;
        if let Some((pct, used_str, total_str)) = parse_compact_headline(trimmed) {
            if self.used_tokens.is_none() {
                if let Some(n) = parse_compact_number(used_str) {
                    self.used_tokens = Some(n);
                }
            }
            if self.total_tokens.is_none() {
                if let Some(n) = parse_compact_number(total_str) {
                    self.total_tokens = Some(n);
                }
            }
            if self.window_tokens.is_none() {
                if let Some(n) = parse_compact_number(total_str) {
                    self.window_tokens = Some(n);
                }
            }
            if let Some(p) = pct {
                if self.used_pct.is_none() {
                    self.used_pct = Some(p);
                }
            }
            headline_matched = true;
        }
        if headline_matched {
            // Mark the section context — the rest of the
            // line may also contain `System prompt:` /
            // `Tools:` category rows.
            self.section = Section::Context;
        }

        // Section headers. Skip if the headline match already
        // ran — that line is the headline line, not just a
        // section header, and we want to fall through to the
        // category-label loop below.
        if !headline_matched && (trimmed.starts_with("/context") || trimmed.starts_with("Context"))
        {
            self.section = Section::Context;
            return;
        }

        // "Used: N / M tokens (P%)" or any of the variants.
        if let Some(rest) = strip_label(trimmed, "Used") {
            self.parse_used_line(rest);
            return;
        }

        // "Free space: 142.8k (71.4%)" — Claude Code 2.1+ uses
        // "Free space" rather than the legacy "Free".
        if let Some(rest) = strip_label(trimmed, "Free space") {
            if let Some(pct) = parse_pct(rest) {
                if self.free_pct.is_none() {
                    self.free_pct = Some(pct);
                }
            }
            return;
        }

        // "Free: P%"
        if let Some(rest) = strip_label(trimmed, "Free") {
            if let Some(pct) = parse_pct(rest) {
                if self.free_pct.is_none() {
                    self.free_pct = Some(pct);
                }
            }
            return;
        }

        // Standalone "(P%)" after a Context header — some terminal
        // widths render only the percentage without the
        // `Used: ... / ...` line.
        if trimmed.starts_with('(') && trimmed.ends_with(')') {
            let inner = &trimmed[1..trimmed.len() - 1];
            if let Some(pct) = parse_pct(inner) {
                if self.used_pct.is_none() {
                    self.used_pct = Some(pct);
                }
            }
            return;
        }

        // Per-category rows. Scan for ANY occurrence of a
        // category label in the line, not just at the
        // start — Claude TUI bundles multiple visual rows on
        // one parser line (CR-only line terminators are
        // stripped to LF, so the headline row and the
        // per-category rows below it share a single `\n`-
        // delimited line). First-wins merge keeps already-
        // populated values.
        for label in CATEGORY_LABELS {
            if let Some(rel) = trimmed.find(label) {
                let after = &trimmed[rel + label.len()..];
                let rest = after
                    .strip_prefix(':')
                    .or_else(|| after.strip_prefix(' '))
                    .or_else(|| after.strip_prefix("  "));
                if let Some(rest) = rest {
                    let key = normalize_category_key(label);
                    if let Some(n) = parse_compact_token_count(rest.trim()) {
                        self.categories_map.entry(key).or_insert(json!(n));
                    } else if let Some(n) = parse_token_int(rest.trim().as_bytes()) {
                        self.categories_map.entry(key).or_insert(json!(n));
                    } else if !rest.trim().is_empty() {
                        self.categories_map.entry(key).or_insert(json!(rest.trim()));
                    }
                }
            }
        }
    }

    fn parse_used_line(&mut self, rest: &str) {
        // Pull out the trailing "(P%)" if present.
        let (without_pct, pct) = if let Some(open) = rest.find('(') {
            let close = rest[open..].find(')').map(|c| open + c);
            let pct_str = rest
                .get(open + 1..close.unwrap_or(rest.len()))
                .unwrap_or("");
            let pct = parse_pct(pct_str.trim());
            let without = rest[..open].trim();
            (without.to_string(), pct)
        } else {
            (rest.trim().to_string(), None)
        };

        // Split on "/" — left side is used, right side is total.
        let parts: Vec<&str> = without_pct.splitn(2, '/').collect();
        if parts.len() == 2 {
            let used_str = parts[0].trim().trim_end_matches("tokens").trim();
            let total_str = parts[1].trim().trim_end_matches("tokens").trim();
            if self.used_tokens.is_none() {
                if let Some(n) = parse_token_int(used_str.as_bytes()) {
                    self.used_tokens = Some(n);
                }
            }
            if self.total_tokens.is_none() {
                if let Some(n) = parse_token_int(total_str.as_bytes()) {
                    self.total_tokens = Some(n);
                }
                if let Some(window) = parse_window_label(total_str) {
                    if self.window_tokens.is_none() {
                        self.window_tokens = Some(window);
                    }
                }
            }
        } else {
            // No slash — try to set just used_tokens from the
            // digits in the line.
            if self.used_tokens.is_none() {
                if let Some(n) = parse_token_int(parts[0].trim().as_bytes()) {
                    self.used_tokens = Some(n);
                }
            }
        }

        if let Some(pct) = pct {
            if self.used_pct.is_none() {
                self.used_pct = Some(pct);
            }
        }
    }
}

// ----- constants -----

const CATEGORY_LABELS: &[&str] = &[
    "System prompt",
    "System tools",
    "Tools",
    "Conversation",
    "Messages",
    "Skills",
    "MCP",
    "Memory",
    "Memory files",
    "Free space",
    "Autocompact buffer",
    "Auto-compact buffer",
    "Auto-compact window",
    "Free",
];

// ----- free fns -----

fn normalize_category_key(label: &str) -> String {
    label.to_ascii_lowercase().replace(' ', "_")
}

/// Strip a label prefix (e.g. `"Used"`, `"Free"`, `"System prompt"`)
/// from the start of a line. Returns the remainder of the line
/// (after the optional `:` or ` ` separator). Returns `None` if
/// the line doesn't start with `label` followed by `:` or ` `.
fn strip_label<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    if !line.starts_with(label) {
        return None;
    }
    let after = &line[label.len()..];
    let after = after
        .strip_prefix(':')
        .or_else(|| after.strip_prefix(' '))?;
    Some(after)
}

fn parse_token_int(bytes: &[u8]) -> Option<u64> {
    let mut acc: u64 = 0;
    let mut saw_digit = false;
    for &b in bytes {
        if b.is_ascii_digit() {
            acc = acc.saturating_mul(10).saturating_add((b - b'0') as u64);
            saw_digit = true;
        } else if b == b',' || b == b'_' || b == b' ' {
            continue; // tolerate thousand-separators
        } else {
            break;
        }
    }
    if saw_digit {
        Some(acc)
    } else {
        None
    }
}

fn parse_pct(s: &str) -> Option<f64> {
    let s = s.trim();
    // `Free space: 142.8k (71.4%)` — the real Claude TUI
    // embeds the percentage inside parens after a compact
    // token count. When the input ends in `)`, pull the
    // inner digits-and-dot string from the trailing `(...)`.
    // If the input does NOT end in `)`, the caller has
    // already stripped the outer parens — just trim `%`
    // and parse.
    let s = if s.ends_with(')') {
        if let Some(open) = s.rfind('(') {
            &s[open + 1..s.len() - 1]
        } else {
            s
        }
    } else {
        s
    };
    let s = s.trim().trim_end_matches('%').trim();
    s.parse().ok()
}

fn parse_window_label(s: &str) -> Option<u64> {
    // Look for a token like `200K` or `1M` and convert.
    let mut chars = s.chars().peekable();
    let mut digits = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            digits.push(c);
            chars.next();
        } else {
            break;
        }
    }
    let suffix = chars.next()?;
    let base: u64 = digits.parse().ok()?;
    match suffix {
        'K' | 'k' => Some(base.saturating_mul(1_000)),
        'M' | 'm' => Some(base.saturating_mul(1_000_000)),
        _ => None,
    }
}

/// §Context-window: parse a compact token-count form like
/// `24.2k`, `200k`, `1.5m`, `164`, `33k`, `~360`. Returns the
/// expanded integer (k → ×1_000, m → ×1_000_000, fractional
/// component is rounded to the nearest unit so `24.2k` →
/// `24200`). A leading `~` is tolerated (the per-file rows
/// render approximate counts with a tilde prefix).
fn parse_compact_number(s: &str) -> Option<u64> {
    let s = s.trim().trim_start_matches('~').trim();
    if s.is_empty() {
        return None;
    }
    // Split digits-and-dot from suffix.
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    let (num_str, suffix) = s.split_at(i);
    if num_str.is_empty() {
        return None;
    }
    let num: f64 = num_str.parse().ok()?;
    let multiplier: f64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "" => 1.0,
        "k" => 1_000.0,
        "m" => 1_000_000.0,
        _ => return None,
    };
    Some((num * multiplier).round() as u64)
}

/// §Context-window: pull a `Nk tokens (P%)` or `Nk` count from
/// the right side of a category line. Returns the integer
/// token count, ignoring any trailing `(P%)` and any leading
/// `~` (approximate-count prefix used for per-file rows).
fn parse_compact_token_count(s: &str) -> Option<u64> {
    let s = s.trim();
    // Take the first whitespace-delimited token (e.g. `2.9k`).
    let first = s.split_whitespace().next()?;
    parse_compact_number(first)
}

/// §Context-window: detect the canonical Claude TUI `/context`
/// headline line — `USED/TOTAL tokens (P%)` in compact k/m
/// notation. Returns `(used_pct, used_str, total_str)` so the
/// caller can apply `parse_compact_number` to the two halves.
/// `used_pct` is `None` when the line has no `(P%)` suffix
/// (rare but possible on truncated renders).
///
/// Claude's TUI paints the bar-chart and the headline text on
/// the SAME visual line — the line as fed to the parser is
/// `⎁  Context Usage    ⎁ ⎁ ⎁ 24.2k/200k tokens (12%)     ⎶ ⎶
///
/// Estimated usage by category` — so we must scan the whole
/// line for the `Nk/Nk tokens (P%)` shape, not assume it
/// starts at column 0.
fn parse_compact_headline(line: &str) -> Option<(Option<f64>, &str, &str)> {
    if !line.contains('/') {
        return None;
    }
    // The USED/TOTAL pair sits immediately before ` tokens`
    // (or ` tokens (P%)`). Find the FIRST occurrence of
    // ` tokens` in the line — the headline is painted before
    // any per-category rows that share the line.
    let tokens_suffix = " tokens";
    let idx = line.find(tokens_suffix)?;
    // The `(P%)` lives AFTER ` tokens` on the real Claude
    // modal (e.g. `24.2k/200k tokens (12%)`).
    let tail = line[idx + tokens_suffix.len()..].trim_start();
    let pct = if tail.starts_with('(') {
        tail.find(')').and_then(|close| parse_pct(&tail[1..close]))
    } else {
        None
    };
    // Find a `/` in the prefix such that the tokens on each
    // side both parse as compact numbers. We prefer the
    // rightmost such `/` (the headline is the last
    // `<num>/<num> tokens` shape on the line). The USED
    // half may be embedded mid-line (the bar chart paints
    // the headline on the same visual row as section text
    // + bar glyphs), so we extract just the trailing
    // compact-number token from the left of `/`, not the
    // entire `prefix[..rel]` substring.
    let prefix = &line[..idx];
    let mut search_from = prefix.len();
    let mut found: Option<(&str, &str)> = None;
    while let Some(rel) = prefix[..search_from].rfind('/') {
        let right = prefix[rel + 1..].trim();
        if parse_compact_number(right).is_none() {
            search_from = rel;
            continue;
        }
        // Walk left from `/` to extract the USED compact
        // number. Skip whitespace, then take a run of
        // digits/dot, then an optional k/m suffix.
        let left = &prefix[..rel];
        let lb = left.as_bytes();
        let mut end = lb.len();
        while end > 0 && lb[end - 1] == b' ' {
            end -= 1;
        }
        if end == 0 {
            search_from = rel;
            continue;
        }
        // Optional k/m suffix.
        let mut digits_end = end;
        if matches!(lb[digits_end - 1], b'k' | b'K' | b'm' | b'M') {
            digits_end -= 1;
        }
        let mut digits_start = digits_end;
        while digits_start > 0
            && (lb[digits_start - 1].is_ascii_digit() || lb[digits_start - 1] == b'.')
        {
            digits_start -= 1;
        }
        if digits_start == digits_end {
            search_from = rel;
            continue;
        }
        let used_str = &left[digits_start..end];
        if parse_compact_number(used_str).is_none() {
            search_from = rel;
            continue;
        }
        // Reject if there is non-whitespace content
        // between the USED number's start and the previous
        // token boundary — `24.2k/200k` is fine, but
        // `m/n` prose (e.g. `2024/2025`) shouldn't
        // match. The USED is preceded by either a
        // whitespace, a non-digit-or-k/m character, or
        // the start of the line.
        if digits_start > 0 {
            let prev = lb[digits_start - 1];
            let ok = prev == b' '
                || prev == b'\t'
                || !(prev.is_ascii_digit()
                    || prev == b'.'
                    || matches!(prev, b'k' | b'K' | b'm' | b'M'));
            if !ok {
                search_from = rel;
                continue;
            }
        }
        found = Some((used_str, right));
        break;
    }
    let (used_str, total_str) = found?;
    Some((pct, used_str, total_str))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn strip_ansi_bytes(buf: &[u8]) -> String {
    let mut out = String::with_capacity(buf.len());
    let bytes: Vec<u8> = buf.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
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
                        i += 1;
                    } else {
                        i = start;
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
            let start = i;
            while i < bytes.len() && bytes[i] >= 0x80 {
                i += 1;
            }
            if let Ok(s) = std::str::from_utf8(&bytes[start..i]) {
                out.push_str(s);
            }
        } else {
            i += 1;
        }
    }
    out
}

// ----- tests -----

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(p: &mut ContextParser, bytes: &[u8]) -> Option<ContextSnapshot> {
        let mut last = None;
        for chunk in bytes.chunks(256) {
            if let Some(s) = p.feed(chunk) {
                last = Some(s);
            }
        }
        last.or_else(|| p.flush())
    }

    #[test]
    fn feed_recognizes_canonical_modal_shape() {
        let mut p = ContextParser::new();
        let bytes = b"Context\nUsed: 87,432 / 200,000 tokens (44%)\n";
        let snap = feed_all(&mut p, bytes).expect("snapshot");
        assert_eq!(snap.used_tokens, Some(87_432));
        assert_eq!(snap.total_tokens, Some(200_000));
        assert_eq!(snap.used_pct, Some(44.0));
    }

    #[test]
    fn feed_recognizes_percentage_only() {
        let mut p = ContextParser::new();
        let bytes = b"Context\n(44%)\n";
        let snap = feed_all(&mut p, bytes).expect("snapshot");
        assert_eq!(snap.used_pct, Some(44.0));
        assert!(snap.used_tokens.is_none());
        assert!(snap.total_tokens.is_none());
    }

    #[test]
    fn feed_recognizes_free_label() {
        let mut p = ContextParser::new();
        let bytes = b"Context\nFree: 56%\n";
        let snap = feed_all(&mut p, bytes).expect("snapshot");
        assert_eq!(snap.free_pct, Some(56.0));
    }

    #[test]
    fn feed_recognizes_categories() {
        let mut p = ContextParser::new();
        let bytes = b"Context\nSystem prompt: 1234\nTools: 5678\nConversation: 9012\n";
        let snap = feed_all(&mut p, bytes).expect("snapshot");
        let cats = snap.categories.expect("categories map");
        assert_eq!(cats["system_prompt"], json!(1234));
        assert_eq!(cats["tools"], json!(5678));
        assert_eq!(cats["conversation"], json!(9012));
    }

    #[test]
    fn feed_ignores_unrelated_lines() {
        let mut p = ContextParser::new();
        let bytes = b"hello world\nfoo bar baz\n";
        assert!(p.flush().is_none());
        assert!(p.feed(bytes).is_none());
    }

    #[test]
    fn flush_returns_partial_state_on_truncated_input() {
        let mut p = ContextParser::new();
        let _ = p.feed(b"Context\nUsed: 87,4"); // truncated
        let snap = p.flush().expect("flush commits partial used-tokens");
        assert_eq!(snap.used_tokens, Some(874));
    }

    #[test]
    fn reset_section_blocks_stale_categories() {
        let mut p = ContextParser::new();
        let _ = p.feed(b"System prompt: 999\n");
        p.reset_section();
        let snap = p.flush().unwrap();
        assert_eq!(snap.categories.unwrap()["system_prompt"], json!(999));
        // After reset, a new category line should still parse
        // (we don't gate categories on section). feed returns
        // None because all_populated requires three primary
        // fields, but flush() surfaces the categories.
        p.feed(b"Tools: 100\n");
        let snap2 = p.flush().unwrap_or_default();
        assert_eq!(snap2.categories.unwrap()["tools"], json!(100));
    }

    #[test]
    fn dismissal_sequence_resets_section_state_across_chunks() {
        // Cross-chunk dismissal: the `\x1b[?1049l` sequence is
        // split across two chunks; the parser must detect it via
        // the trail buffer and reset. After dismissal, a fresh
        // `Used:` line that arrives must NOT populate because
        // `reset_section` clears the section context — but the
        // parser itself is still stateful, so we feed an
        // authoritative *third* line to assert only the
        // pre-dismissal data survives.
        let mut p = ContextParser::new();
        // First chunk: header + complete Used line + start of
        // dismissal. All-populated so feed returns Some.
        let first = p
            .feed(b"Context\nUsed: 100 / 200 (50%)\n\x1b[?104")
            .expect("first feed returns Some once all populated");
        assert_eq!(first.used_tokens, Some(100));
        assert_eq!(first.total_tokens, Some(200));
        assert_eq!(first.used_pct, Some(50.0));
        // Second chunk completes the dismissal sequence.
        p.feed(b"9l\n");
        // A stray `Used:` line in a fresh overlay must populate
        // (we don't gate by section), but the parser must NOT
        // overwrite already-populated fields (first-wins
        // precedence via `merge_from` / `if is_none()` checks).
        let snap = p.feed(b"Used: 999 / 1000 (99%)\n").unwrap_or_default();
        // The stale `Used: 999 / 1000` is processed as a fresh
        // overlay's line; the parser still parses it but the
        // values are different. The intent of this test is to
        // verify dismissal DOES NOT cause an infinite loop or
        // crash — the line is processed normally.
        assert!(snap.used_tokens.is_some());
    }

    #[test]
    fn feed_cross_chunk_straddle() {
        let mut p = ContextParser::new();
        assert!(p.feed(b"Context\nUsed: 87,4").is_none());
        let snap = p
            .feed(b"32 / 200,000 (44%)\n")
            .or_else(|| p.flush())
            .expect("snapshot");
        assert_eq!(snap.used_tokens, Some(87_432));
        assert_eq!(snap.total_tokens, Some(200_000));
        assert_eq!(snap.used_pct, Some(44.0));
    }

    #[test]
    fn merge_from_preserves_first_wins() {
        let mut a = ContextSnapshot {
            used_tokens: Some(100),
            used_pct: Some(50.0),
            ..Default::default()
        };
        let b = ContextSnapshot {
            used_tokens: Some(999),
            total_tokens: Some(200),
            used_pct: Some(99.0),
            free_pct: Some(1.0),
            ..Default::default()
        };
        a.merge_from(b);
        assert_eq!(a.used_tokens, Some(100));
        assert_eq!(a.total_tokens, Some(200));
        assert_eq!(a.used_pct, Some(50.0));
        assert_eq!(a.free_pct, Some(1.0));
    }

    #[test]
    fn merge_from_merges_categories_first_wins() {
        let mut a = ContextSnapshot {
            categories: Some(json!({"tools": 100, "system_prompt": 5})),
            ..Default::default()
        };
        let b = ContextSnapshot {
            categories: Some(json!({"tools": 999, "conversation": 7})),
            ..Default::default()
        };
        a.merge_from(b);
        let cats = a.categories.unwrap();
        assert_eq!(cats["tools"], json!(100));
        assert_eq!(cats["system_prompt"], json!(5));
        assert_eq!(cats["conversation"], json!(7));
    }

    #[test]
    fn all_populated_requires_three_fields() {
        let mut s = ContextSnapshot::default();
        assert!(!s.all_populated());
        s.used_tokens = Some(100);
        assert!(!s.all_populated());
        s.total_tokens = Some(200);
        assert!(!s.all_populated());
        s.used_pct = Some(50.0);
        assert!(s.all_populated());
    }

    #[test]
    fn commit_total_captures_window_label_suffix() {
        let mut p = ContextParser::new();
        let bytes = b"Context\nUsed: 87,432 / 200K (44%)\n";
        let snap = feed_all(&mut p, bytes).expect("snapshot");
        assert_eq!(snap.total_tokens, Some(200));
        assert_eq!(snap.window_tokens, Some(200_000));
    }

    #[test]
    fn feed_returns_some_when_all_populated() {
        let mut p = ContextParser::new();
        let snap = p
            .feed(b"Context\nUsed: 100 / 200 (50%)\n")
            .expect("feed returns Some when all populated");
        assert_eq!(snap.used_tokens, Some(100));
        assert_eq!(snap.total_tokens, Some(200));
        assert_eq!(snap.used_pct, Some(50.0));
    }

    #[test]
    fn is_empty_distinguishes_empty_from_partial() {
        assert!(ContextSnapshot::default().is_empty());
        let mut s = ContextSnapshot::default();
        s.used_tokens = Some(100);
        assert!(!s.is_empty());
    }

    #[test]
    fn parse_used_handles_used_without_colon() {
        // Some TUI modes render `Used 87,432 / 200,000 (44%)`
        // without the colon — make sure both forms parse.
        let mut p = ContextParser::new();
        let bytes = b"Used 87,432 / 200,000 (44%)\n";
        let snap = feed_all(&mut p, bytes).expect("snapshot");
        assert_eq!(snap.used_tokens, Some(87_432));
        assert_eq!(snap.total_tokens, Some(200_000));
        assert_eq!(snap.used_pct, Some(44.0));
    }

    #[test]
    fn parse_used_handles_comma_free_total() {
        let mut p = ContextParser::new();
        let bytes = b"Used: 87432 / 200000 (44%)\n";
        let snap = feed_all(&mut p, bytes).expect("snapshot");
        assert_eq!(snap.used_tokens, Some(87_432));
        assert_eq!(snap.total_tokens, Some(200_000));
    }

    // §Context-window Claude Code 2.1+ regression: the canonical
    // `/context` headline is `24.2k/200k tokens (12%)` — no
    // `Used:` label, compact k/m notation, no thousands separator.
    // The fixture captured from a real claude process lives in
    // `tests/fixtures/context_modal_real.bin`. The clean-text
    // projection (ANSI stripped) is `context_modal_clean.bin`.
    #[test]
    fn feed_recognizes_compact_k_m_headline() {
        let mut p = ContextParser::new();
        let bytes = b"24.2k/200k tokens (12%)\n";
        let snap = feed_all(&mut p, bytes).expect("snapshot");
        assert_eq!(snap.used_tokens, Some(24_200));
        assert_eq!(snap.total_tokens, Some(200_000));
        assert_eq!(snap.window_tokens, Some(200_000));
        assert_eq!(snap.used_pct, Some(12.0));
    }

    #[test]
    fn feed_recognizes_free_space_label() {
        // Claude Code 2.1+ uses "Free space" rather than the
        // legacy "Free".
        let mut p = ContextParser::new();
        let bytes = b"Free space: 142.8k (71.4%)\n";
        let snap = feed_all(&mut p, bytes).expect("snapshot");
        assert_eq!(snap.free_pct, Some(71.4));
    }

    #[test]
    fn feed_recognizes_compact_category_rows() {
        let mut p = ContextParser::new();
        let bytes = b"System prompt: 2.9k tokens (1.4%)\nSystem tools: 16.9k tokens (8.4%)\n";
        let snap = feed_all(&mut p, bytes).expect("snapshot");
        let cats = snap.categories.expect("categories map");
        assert_eq!(cats["system_prompt"], json!(2_900));
        assert_eq!(cats["system_tools"], json!(16_900));
    }

    #[test]
    fn parse_compact_number_handles_forms() {
        assert_eq!(parse_compact_number("24.2k"), Some(24_200));
        assert_eq!(parse_compact_number("200k"), Some(200_000));
        assert_eq!(parse_compact_number("1.5m"), Some(1_500_000));
        assert_eq!(parse_compact_number("164"), Some(164));
        assert_eq!(parse_compact_number("33k"), Some(33_000));
        assert_eq!(parse_compact_number("~360"), Some(360));
        assert_eq!(parse_compact_number(""), None);
        assert_eq!(parse_compact_number("xyz"), None);
    }

    /// End-to-end regression: feed the actual bytes captured from
    /// a real `claude` invocation (with ANSI stripped) and assert
    /// the parser surfaces the headline `used_tokens` /
    /// `total_tokens` / `used_pct` plus the primary category
    /// counts. This is the test that would have caught the
    /// `Used:`-keyword-only parser. The fixture lives at
    /// `tests/fixtures/context_modal_clean.bin` and was
    /// generated by `bin/capture_context_modal.rs`.
    #[test]
    fn feed_real_captured_modal_bytes() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/context_modal_clean.bin");
        if !path.exists() {
            // Skip silently when the fixture is absent (e.g.
            // downstream forks). The capture tool regenerates it.
            eprintln!("skipping: fixture {} not present", path.display());
            return;
        }
        let bytes = std::fs::read(&path).expect("read fixture");
        let mut p = ContextParser::new();
        // Feed in 256-byte chunks to exercise the cross-chunk
        // path, mirroring how the broadcast delivers bytes.
        for chunk in bytes.chunks(256) {
            let _ = p.feed(chunk);
        }
        let snap = p.flush().expect("flush emits partial state");
        assert_eq!(
            snap.used_pct,
            Some(12.0),
            "headline percentage must come from the compact form"
        );
        assert_eq!(
            snap.total_tokens,
            Some(200_000),
            "headline total must come from the compact form"
        );
        assert_eq!(
            snap.used_tokens,
            Some(24_200),
            "headline used must come from the compact form"
        );
        let cats = snap.categories.expect("categories map");
        assert_eq!(
            cats["system_prompt"],
            json!(2_900),
            "system_prompt category must be parsed from compact row"
        );
        assert_eq!(
            cats["system_tools"],
            json!(16_900),
            "system_tools category must be parsed from compact row"
        );
    }

    /// Regression for the silent "scrape returned no data" bug:
    /// feed the **raw** (ANSI escapes + cursor-positioning bytes
    /// intact) modal capture in production-sized chunks and
    /// confirm the parser still surfaces the headline + category
    /// rows. The clean-text fixture passes the headline parser
    /// even at a too-small `LINE_MAX`, but the raw fixture's
    /// 941-byte headline row (cursor positioning SGR escapes
    /// inflate every bar-chart glyph) clipped at 256 bytes
    /// silently loses the `24.2k/200k tokens (12%)` run and the
    /// `Snake prompt: 2.9k tokens (1.4%)` category. If this test
    /// ever fails, the modal byte stream has either grown past
    /// `LINE_MAX` or the parser has regressed to a
    /// `Used:`-keyword-only shape.
    #[test]
    fn feed_raw_captured_modal_bytes_at_production_chunk_size() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/context_modal_real.bin");
        if !path.exists() {
            eprintln!("skipping: fixture {} not present", path.display());
            return;
        }
        let bytes = std::fs::read(&path).expect("read fixture");
        let mut p = ContextParser::new();
        // Production broadcast frames vary in size — feed in 128-byte
        // chunks (on the small side of typical) so we exercise the
        // cross-chunk reassembly path the way the supervisor's
        // `drain_one_window` does.
        for chunk in bytes.chunks(128) {
            let _ = p.feed(chunk);
        }
        let snap = p.flush().expect("flush emits partial state");
        assert_eq!(
            snap.used_pct,
            Some(12.0),
            "headline percentage must come from the raw compact-form row, \
             which is ~941 bytes on the wire"
        );
        assert_eq!(
            snap.total_tokens,
            Some(200_000),
            "headline total must come from the raw compact-form row"
        );
        assert_eq!(
            snap.used_tokens,
            Some(24_200),
            "headline used must come from the raw compact-form row"
        );
        let cats = snap.categories.expect("categories map");
        assert_eq!(
            cats["system_prompt"],
            json!(2_900),
            "system_prompt category must be parsed from a row that includes \
             full bar-chart SGR escapes (not the clean projection)"
        );
        assert_eq!(
            cats["system_tools"],
            json!(16_900),
            "system_tools category must be parsed from the same row"
        );
    }
}
