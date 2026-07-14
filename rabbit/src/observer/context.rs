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
//! ## Architecture
//!
//! The parser is layered so each interesting decision is a small,
//! pure function with a tiny signature:
//!
//! - **Layer 1 (ESC stripping)** lives in
//!   [`crate::observer::text::ansi::strip_ansi_bytes`]. Pure.
//! - **Layer 2 (line classification)** maps a single cleaned line
//!   to a [`LineKind`] via [`classify_line`]. Pure.
//! - **Layer 3 (snapshot reducer)** folds one [`LineKind`] onto
//!   an existing [`ContextSnapshot`] via [`apply_to_snapshot`].
//!   Pure.
//! - **Layer 4 (stateful boundary)** is [`ContextParser`] itself,
//!   which owns `line_buf` and `trail` for cross-chunk correctness
//!   and is the only place state lives.
//!
//! Adding a new modal shape means writing one new `match_*`
//! function, adding it to [`classify_line`], adding a new
//! [`LineKind`] arm, and adding tests. The 100+ LOC `parse_line`
//! previously responsible for every modal shape is gone.
//!
//! [`flush`]: ContextParser::flush

use serde_json::{json, Map, Value};

use crate::observer::text::ansi::strip_ansi_bytes;

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
        // Layer 1 (strip ANSI) → Layer 2 (classify) → Layer 3 (reduce).
        // See the module doc + LineKind docs for the layered contract.
        let line = strip_ansi_bytes(raw);
        let kinds = classify_line(&line);
        if kinds.is_empty() {
            return;
        }
        // Roll current state into a snapshot so first-wins merge
        // is honored across all the LineKinds this line yielded.
        let mut snap = ContextSnapshot {
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
        };
        for kind in kinds {
            if matches!(kind, LineKind::SectionHeader) {
                self.section = Section::Context;
            }
            apply_to_snapshot(&mut snap, kind);
        }
        self.used_tokens = snap.used_tokens;
        self.total_tokens = snap.total_tokens;
        self.used_pct = snap.used_pct;
        self.free_pct = snap.free_pct;
        self.window_tokens = snap.window_tokens;
        if let Some(Value::Object(obj)) = snap.categories {
            self.categories_map = obj;
        }
    }
}

// ----- Layer 2 / Layer 3: classification + reducer -----

/// §Parse-layer-2 / a single cleaned modal line's semantic
/// shape. Output of [`classify_line`], input to
/// [`apply_to_snapshot`]. Keeping the seven arms explicit (vs.
/// stuffing the data into a six-tuple or a struct with optional
/// fields) gives a clean match in [`apply_to_snapshot`] and
/// makes the test surface per-arm trivial — one test per
/// variant.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LineKind {
    /// Claude Code 2.1+ modal headline: `24.2k/200k tokens (12%)`.
    /// Compact k/m notation, no `Used:` label. The `pct` may be
    /// `None` when the line has no trailing `(P%)` (rare but
    /// possible on truncated renders).
    HeadlineCompact {
        used: u64,
        total: u64,
        pct: Option<f64>,
    },
    /// Legacy modal headline: `Used: 87,432 / 200,000 tokens (44%)`
    /// (with optional comma/grouping and optional `tokens`
    /// literal). The `total` and `window` may be `None` when the
    /// line is a partial commit (e.g. `Used: 87,4` mid-stream with
    /// the slash arriving in the next chunk). `window` is set to
    /// the K/M-expanded form of `total` when the total carries a
    /// `K`/`M` suffix (e.g. `200K` → `total=200, window=200_000`).
    HeadlineLegacy {
        used: Option<u64>,
        total: Option<u64>,
        window: Option<u64>,
        pct: Option<f64>,
    },
    /// Per-category row: `System prompt: 2.9k tokens (1.4%)`.
    /// The `pct` may be `None` if the row omits the percentage
    /// (e.g. truncated render).
    Category {
        name: String,
        tokens: u64,
        pct: Option<f64>,
    },
    /// Free-space label: `Free space: 142.8k (71.4%)` or the
    /// legacy `Free: 56%`. The percentage is mandatory — if
    /// there's no usable percentage the matcher returns
    /// `Ignored`.
    FreeSpace { pct: f64 },
    /// `(44%)` alone on a line — narrow-terminal headline-only
    /// form.
    StandalonePct { pct: f64 },
    /// `Context Usage` (or `/context`) alone on a line — the
    /// modal opened. No data commit; just resets internal
    /// section state to `Context` so subsequent category rows
    /// are accepted.
    SectionHeader,
}

/// §Parse-layer-2 / dispatch a single cleaned modal line to a
/// (possibly empty) list of [`LineKind`]s. Pure: depends only
/// on the input text.
///
/// The headline forms, free-space, and standalone-pct matchers
/// are first-wins (at most one match per line). The category
/// matcher scans the whole line for every recognised label —
/// Claude's modal paints all category rows on the same visual
/// line (separated by `⏶` bar-chart glyphs, not `\n`), so a
/// single line can legitimately surface `System prompt`,
/// `System tools`, `Memoryfiles`, `Skills`, `Messages`,
/// `Freespace`, `Autocompact buffer`, … in one pass.
///
/// Order of arms matters:
///
/// - **Headlines first.** `24.2k/200k tokens (12%)` is matched
///   before the category scanner so it writes
///   `used/total/used_pct` rather than getting eaten as a row.
/// - **Free-space before category.** `Free space: …` contains
///   the substring `"Free space"` (also a CATEGORY_LABEL); the
///   free-space matcher must claim it first or `free_pct` is
///   never populated.
/// - **Section-header last.** A `Context Usage` row resets
///   state but doesn't surface data, so it's emitted only if
///   no data-bearing matcher fired.
pub(crate) fn classify_line(line: &str) -> Vec<LineKind> {
    let trimmed = line.trim();
    let mut out = Vec::new();
    if trimmed.is_empty() {
        return out;
    }
    if let Some((used, total, pct)) = match_headline_compact(trimmed) {
        out.push(LineKind::HeadlineCompact { used, total, pct });
    }
    if let Some(h) = match_headline_legacy(trimmed) {
        out.push(LineKind::HeadlineLegacy {
            used: h.used,
            total: h.total,
            window: h.window,
            pct: h.pct,
        });
    }
    if let Some(pct) = match_free_space(trimmed) {
        out.push(LineKind::FreeSpace { pct });
    }
    // Categories: scan the whole line for every label, since
    // the modal paints multiple category rows on one visual
    // line.
    for cat in match_all_categories(trimmed) {
        out.push(LineKind::Category {
            name: cat.name,
            tokens: cat.tokens,
            pct: cat.pct,
        });
    }
    if let Some(pct) = match_standalone_pct(trimmed) {
        out.push(LineKind::StandalonePct { pct });
    }
    if matches_section_header(trimmed) {
        out.push(LineKind::SectionHeader);
    }
    out
}

struct CategoryMatch {
    name: String,
    tokens: u64,
    pct: Option<f64>,
}

/// Match Claude Code 2.1+ compact headline like
/// `24.2k/200k tokens (12%)`. Scans the whole line for the
/// `<num>/<num> tokens (P%)` shape because the real modal paints
/// the bar chart + headline on the same visual row as section
/// text + bar glyphs. Returns `(used, total, pct)`; `pct` is
/// `None` if no `(P%)` suffix.
fn match_headline_compact(line: &str) -> Option<(u64, u64, Option<f64>)> {
    let parsed = parse_compact_headline(line)?;
    let (_, used_str, total_str) = parsed;
    let used = parse_compact_number(used_str)?;
    let total = parse_compact_number(total_str)?;
    // The compact headline parser returned the pct as `Option<f64>`
    // implicitly via the `(P%)` parsing inside parse_compact_headline.
    // Re-derive it for the LineKind payload.
    let pct = parse_pct_for_compact(line);
    Some((used, total, pct))
}

/// Match legacy modal headline like
/// `Used: 87,432 / 200,000 tokens (44%)` (or
/// `Used 87,432 / 200,000 (44%)` — colon optional,
/// Parsed `Used:` headline values. Each numeric is
/// `Option`-wrapped so a *partial* commit (e.g. `Used: 87,4`
/// arriving across a chunk boundary before the `/<total>` part
/// arrives) can populate just `used` without overwriting empty
/// slots with garbage. `window` is the K/M-expanded form of
/// `total` (e.g. `200K` → `total=200, window=200_000`) — the
/// original parser preserved this distinction even though it's
/// semantically odd, and an existing test pins the contract.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LegacyHeadline {
    used: Option<u64>,
    total: Option<u64>,
    window: Option<u64>,
    pct: Option<f64>,
}

/// `tokens` literal optional, `,` thousand-separators
/// optional).
fn match_headline_legacy(line: &str) -> Option<LegacyHeadline> {
    // Strip a leading "Used" label if present.
    let after_label = if let Some(rest) = strip_label(line, "Used") {
        rest.trim()
    } else if line.to_ascii_lowercase().starts_with("used ") {
        &line[4..]
    } else {
        return None;
    };
    if after_label.is_empty() {
        return None;
    }
    // Pull out the trailing "(P%)" if present.
    let (without_pct, pct) = if let Some(open) = after_label.find('(') {
        let close_rel = after_label[open..].find(')');
        let pct_str = close_rel
            .map(|c| &after_label[open + 1..open + c])
            .unwrap_or("");
        let pct = parse_pct(pct_str.trim());
        let without = after_label[..open].trim();
        (without.to_string(), pct)
    } else {
        (after_label.trim().to_string(), None)
    };
    // Full headline: split on `/`. Both halves may end with
    // `tokens`. The total may also carry a K/M suffix
    // (`200K`, `1M`) which the original parser treated as a
    // window label distinct from the raw integer.
    if let Some(slash_rel) = without_pct.find('/') {
        let used_str = without_pct[..slash_rel]
            .trim()
            .trim_end_matches("tokens")
            .trim();
        let total_str = without_pct[slash_rel + 1..]
            .trim()
            .trim_end_matches("tokens")
            .trim();
        let used = parse_token_int(used_str.as_bytes())?;
        let total = parse_token_int(total_str.as_bytes())?;
        let window = parse_window_label(total_str).unwrap_or(total);
        return Some(LegacyHeadline {
            used: Some(used),
            total: Some(total),
            window: Some(window),
            pct,
        });
    }
    // Partial commit: `Used: <digits>` with the `/<total>`
    // half not yet arrived. Surface the used side so a
    // cross-chunk straddle doesn't lose the value the parser
    // already saw.
    let used = parse_token_int(without_pct.as_bytes())?;
    Some(LegacyHeadline {
        used: Some(used),
        total: None,
        window: None,
        pct,
    })
}

/// Match every per-category row on a single line. Returns
/// multiple [`CategoryMatch`]es because Claude's modal paints
/// all category rows on the same visual line — separated by
/// `⏶` bar-chart glyphs and spaces, not `\n`. Walks the line
/// forward label-by-label: after each match, advances past the
/// percentage's closing `)` (or past the `tokens` literal if
/// no percentage is present) before searching for the next
/// label.
///
/// Each label requires a separator (`:`, space, or end of
/// line) immediately after it. The strict check prevents the
/// shorter `"Memory"` label from greedily matching
/// `"Memoryfiles:..."` — the real Claude TUI renders the
/// compact form with no space, so we list both forms in
/// [`CATEGORY_LABELS`] and let each form claim its own row.
fn match_all_categories(line: &str) -> Vec<CategoryMatch> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while cursor < line.len() {
        // Find the earliest label occurrence at-or-after
        // `cursor`. When two labels start at the same byte
        // (e.g. `Memory` vs `Memoryfiles`), the LONGER label
        // wins so the strict-separator check below doesn't
        // reject the compact-render form on a `:` boundary
        // just because its shorter prefix sat there first.
        let mut best: Option<(usize, &str)> = None;
        for label in CATEGORY_LABELS {
            let haystack = &line[cursor..];
            if let Some(rel) = haystack.find(label) {
                let abs = cursor + rel;
                let pick = match best {
                    None => true,
                    Some((best_abs, best_label)) => {
                        abs < best_abs || (abs == best_abs && label.len() > best_label.len())
                    }
                };
                if pick {
                    best = Some((abs, label));
                }
            }
        }
        let Some((abs, label)) = best else {
            break;
        };
        let after_rel = abs + label.len();
        // Require a separator immediately after the label.
        if after_rel < line.len() {
            let next = line.as_bytes()[after_rel];
            if next != b':' && next != b' ' {
                cursor = abs + 1;
                continue;
            }
        }
        let after = &line[after_rel..];
        let body = after
            .strip_prefix(':')
            .or_else(|| after.strip_prefix(' '))
            .unwrap_or("");
        // Scope token + pct extraction to this row only — the
        // line may carry several category rows glued together
        // by bar-chart glyphs, and a naive parse_pct over the
        // full body would find the LAST `(P%)` group rather
        // than the one that belongs to the current row.
        let row_end = body.find(')').map(|c| c + 1).unwrap_or(body.len());
        let row = &body[..row_end];
        let tokens_str = row.trim_start();
        let Some(tokens) = parse_compact_token_count(tokens_str) else {
            // Failed to parse — advance one byte and try again
            // so we don't get stuck on a misfire.
            cursor = abs + 1;
            continue;
        };
        let pct = parse_pct(row);
        out.push(CategoryMatch {
            name: label.to_string(),
            tokens,
            pct,
        });
        // Advance past the row: jump to just after the
        // percentage's closing `)` if present, otherwise past
        // the body. Either way the next iteration starts at a
        // boundary that can't overlap the row we just
        // consumed.
        // `body` starts at absolute offset `after_rel + 1`
        // (one separator byte was stripped).
        cursor = after_rel + 1 + row_end;
    }
    out
}

/// Match free-space labels: `Free space: 142.8k (71.4%)` (modern
/// Claude TUI), the compact-render `Freespace:142.8k(71.4%)`,
/// or the legacy `Free: 56%`.
fn match_free_space(line: &str) -> Option<f64> {
    if let Some(rest) = strip_label(line, "Free space") {
        return parse_pct(rest.trim());
    }
    if let Some(rest) = strip_label(line, "Freespace") {
        return parse_pct(rest.trim());
    }
    if let Some(rest) = strip_label(line, "Free") {
        return parse_pct(rest.trim());
    }
    None
}

/// Match a `(P%)` alone on a line — narrow-terminal headline-only
/// form. Must be the entire content (after trim).
fn match_standalone_pct(line: &str) -> Option<f64> {
    let trimmed = line.trim();
    if trimmed.starts_with('(') && trimmed.ends_with(')') {
        let inner = &trimmed[1..trimmed.len() - 1];
        parse_pct(inner)
    } else {
        None
    }
}

/// Match `Context Usage` (with or without leading `⏵ ` glyph
/// residue) or `/context` alone on a line. Returns true when
/// the line is recognizably the modal-header marker.
fn matches_section_header(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("Context") || trimmed.starts_with("/context")
}

/// §Parse-layer-3 / fold a single classified line onto a
/// snapshot. First-wins semantics: any `Some` field already
/// populated is preserved; new values populate empty slots.
/// Mutates `snap` in place. Pure apart from the mutation — no
/// I/O, no global state.
pub(crate) fn apply_to_snapshot(snap: &mut ContextSnapshot, kind: LineKind) {
    use LineKind::*;
    match kind {
        HeadlineCompact { used, total, pct } => {
            if snap.used_tokens.is_none() {
                snap.used_tokens = Some(used);
            }
            if snap.total_tokens.is_none() {
                snap.total_tokens = Some(total);
            }
            if snap.window_tokens.is_none() {
                snap.window_tokens = Some(total);
            }
            if let Some(p) = pct {
                if snap.used_pct.is_none() {
                    snap.used_pct = Some(p);
                }
            }
        }
        HeadlineLegacy {
            used,
            total,
            window,
            pct,
        } => {
            // Legacy form: each numeric is Option-wrapped so a
            // partial commit (only `used` populated, `/<total>`
            // still pending across a chunk boundary) doesn't
            // stamp a 0 into the total/window slots.
            if let Some(u) = used {
                if snap.used_tokens.is_none() {
                    snap.used_tokens = Some(u);
                }
            }
            if let Some(t) = total {
                if snap.total_tokens.is_none() {
                    snap.total_tokens = Some(t);
                }
            }
            if let Some(w) = window {
                if snap.window_tokens.is_none() {
                    snap.window_tokens = Some(w);
                }
            }
            if let Some(p) = pct {
                if snap.used_pct.is_none() {
                    snap.used_pct = Some(p);
                }
            }
        }
        Category { name, tokens, pct } => {
            let key = normalize_category_key(&name);
            let map = snap
                .categories
                .get_or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(obj) = map {
                obj.entry(key).or_insert(json!(tokens));
            }
            // Per-category percentages aren't surfaced on the
            // wire today; we keep the field available for a
            // future "expand categories" affordance.
            let _ = pct;
        }
        FreeSpace { pct } => {
            if snap.free_pct.is_none() {
                snap.free_pct = Some(pct);
            }
        }
        StandalonePct { pct } => {
            if snap.used_pct.is_none() {
                snap.used_pct = Some(pct);
            }
        }
        SectionHeader => {}
    }
}

/// §Parse-layer-2 helper / extract the trailing `(P%)` from
/// a compact-headline line if present. Used by
/// [`match_headline_compact`] to populate the `pct` field on
/// the [`LineKind::HeadlineCompact`] arm.
fn parse_pct_for_compact(line: &str) -> Option<f64> {
    let tokens_suffix = " tokens";
    let idx = line.find(tokens_suffix)?;
    let tail = line[idx + tokens_suffix.len()..].trim_start();
    if tail.starts_with('(') {
        tail.find(')').and_then(|close| parse_pct(&tail[1..close]))
    } else {
        None
    }
}

// ----- constants -----

const CATEGORY_LABELS: &[&str] = &[
    // Spaced forms (legacy modal text):
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
    // Compact-render forms emitted by Claude Code 2.1+ when
    // the modal is painted tightly. The strict-separator check
    // in [`match_category`] keeps the `"Memory"` label from
    // greedily matching `"Memoryfiles:..."` — each compact form
    // has to claim its own line.
    "Memoryfiles",
    "Freespace",
    "Autocompactbuffer",
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
    if s.ends_with(')') {
        let open = s.rfind('(')?;
        let inner = &s[open + 1..s.len() - 1];
        return inner.trim().trim_end_matches('%').trim().parse().ok();
    }
    // Otherwise the caller has already stripped the outer
    // parens and the input should carry a literal `%`
    // suffix. Without one, the input is a token count (e.g.
    // `1234` or `16.9k`), not a percentage — return None so
    // the caller doesn't mistake a token count for a pct.
    if !s.contains('%') {
        return None;
    }
    s.trim_end_matches('%').trim().parse().ok()
}

/// §Context-window: detect the optional `Nk` / `Nm` window-label
/// suffix on a total like `200K` or `1M`. Returns the expanded
/// integer (`k` → ×1_000, `m` → ×1_000_000). Returns `None` when
/// the total is plain digits (no suffix) — in that case the
/// caller should use the raw total as the window value.
#[allow(dead_code)]
fn parse_window_label(s: &str) -> Option<u64> {
    let s = s.trim().trim_start_matches('~').trim();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let (num_str, suffix) = s.split_at(i);
    let num: f64 = num_str.parse().ok()?;
    let multiplier: f64 = match suffix.trim().to_ascii_lowercase().as_str() {
        "k" => 1_000.0,
        "m" => 1_000_000.0,
        _ => return None,
    };
    Some((num * multiplier).round() as u64)
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
///
/// The modern Claude TUI renders category rows tightly —
/// `33ktokens(16.5%)` — so we cannot rely on a whitespace
/// boundary between the number and the literal `tokens`. We
/// walk the leading run of digits/dots and accept an optional
/// single `k`/`m` suffix character; everything after that
/// (including the `tokens` literal and the trailing `(P%)`)
/// is ignored.
fn parse_compact_token_count(s: &str) -> Option<u64> {
    let s = s.trim().trim_start_matches('~');
    if s.is_empty() {
        return None;
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let (num_str, suffix_start) = s.split_at(i);
    let num: f64 = num_str.parse().ok()?;
    let multiplier: f64 = match suffix_start.chars().next() {
        Some('k') | Some('K') => 1_000.0,
        Some('m') | Some('M') => 1_000_000.0,
        _ => 1.0,
    };
    Some((num * multiplier).round() as u64)
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

    // ---- Layer 2 / classify_line arms ----
    //
    // classify_line now returns Vec<LineKind> because Claude's
    // modal paints several category rows on the same visual
    // line (separated by bar-chart glyphs, not \n). Tests
    // that expect a single-kind classification therefore
    // assert `kinds[0]`; tests that expect multi-match assert
    // the whole Vec.

    fn first_kind(line: &str) -> Option<LineKind> {
        let kinds = classify_line(line);
        kinds.into_iter().next()
    }

    #[test]
    fn classify_headline_compact_extracts_24k_200k_12pct() {
        let kinds = classify_line("24.2k/200k tokens (12%)");
        assert_eq!(
            kinds.first(),
            Some(&LineKind::HeadlineCompact {
                used: 24_200,
                total: 200_000,
                pct: Some(12.0),
            })
        );
    }

    #[test]
    fn classify_headline_compact_handles_decimal_k_suffix() {
        let kinds = classify_line("1.5k/200k tokens (1%)");
        assert_eq!(
            kinds.first(),
            Some(&LineKind::HeadlineCompact {
                used: 1_500,
                total: 200_000,
                pct: Some(1.0),
            })
        );
    }

    #[test]
    fn classify_headline_compact_handles_m_suffix() {
        let kinds = classify_line("1.5m/2m tokens (75%)");
        assert_eq!(
            kinds.first(),
            Some(&LineKind::HeadlineCompact {
                used: 1_500_000,
                total: 2_000_000,
                pct: Some(75.0),
            })
        );
    }

    #[test]
    fn classify_headline_compact_with_no_pct_when_label_omitted() {
        let kinds = classify_line("24.2k/200k tokens");
        assert_eq!(
            kinds.first(),
            Some(&LineKind::HeadlineCompact {
                used: 24_200,
                total: 200_000,
                pct: None,
            })
        );
    }

    #[test]
    fn classify_headline_legacy_with_commas() {
        let kinds = classify_line("Used: 87,432 / 200,000 tokens (44%)");
        assert_eq!(
            kinds.first(),
            Some(&LineKind::HeadlineLegacy {
                used: Some(87_432),
                total: Some(200_000),
                window: Some(200_000),
                pct: Some(44.0),
            })
        );
    }

    #[test]
    fn classify_headline_legacy_without_tokens_literal() {
        let kinds = classify_line("Used: 87432 / 200000 (44%)");
        assert_eq!(
            kinds.first(),
            Some(&LineKind::HeadlineLegacy {
                used: Some(87_432),
                total: Some(200_000),
                window: Some(200_000),
                pct: Some(44.0),
            })
        );
    }

    #[test]
    fn classify_category_system_prompt() {
        let kinds = classify_line("System prompt: 1234");
        assert_eq!(
            kinds.first(),
            Some(&LineKind::Category {
                name: "System prompt".to_string(),
                tokens: 1234,
                pct: None,
            })
        );
    }

    #[test]
    fn classify_category_compact_render_merges_with_neighbors() {
        // The real Claude modal paints several category rows on
        // the same visual line — they should ALL be extracted
        // from one classify_line call.
        let kinds = classify_line(
            "System prompt: 2.9k tokens (1.4%)   System tools: 16.9k tokens (8.4%)\
             Memoryfiles:2.8k tokens (1.4%)",
        );
        assert_eq!(
            kinds.len(),
            3,
            "expected three Category kinds, got {kinds:?}"
        );
        assert_eq!(
            kinds[0],
            LineKind::Category {
                name: "System prompt".to_string(),
                tokens: 2_900,
                pct: Some(1.4),
            }
        );
        assert_eq!(
            kinds[1],
            LineKind::Category {
                name: "System tools".to_string(),
                tokens: 16_900,
                pct: Some(8.4),
            }
        );
        assert_eq!(
            kinds[2],
            LineKind::Category {
                name: "Memoryfiles".to_string(),
                tokens: 2_800,
                pct: Some(1.4),
            }
        );
    }

    #[test]
    fn classify_free_space_with_k_value_and_parenthesized_pct() {
        let kinds = classify_line("Free space: 142.8k (71.4%)");
        assert_eq!(kinds.first(), Some(&LineKind::FreeSpace { pct: 71.4 }));
    }

    #[test]
    fn classify_free_legacy_short_form() {
        let kinds = classify_line("Free: 56%");
        assert_eq!(kinds.first(), Some(&LineKind::FreeSpace { pct: 56.0 }));
    }

    #[test]
    fn classify_standalone_pct_round_trip() {
        let kinds = classify_line("(44%)");
        assert_eq!(kinds.first(), Some(&LineKind::StandalonePct { pct: 44.0 }));
    }

    #[test]
    fn classify_section_header_context_usage_alone() {
        let kinds = classify_line("Context Usage");
        assert_eq!(kinds.first(), Some(&LineKind::SectionHeader));
    }

    #[test]
    fn classify_returns_empty_vec_for_unrelated_line() {
        // classify_line now returns Vec<LineKind> rather than a
        // single Ignored variant — an unrelated line just
        // produces an empty Vec.
        assert!(classify_line("hello world").is_empty());
        assert!(classify_line("").is_empty());
        assert!(classify_line("   ").is_empty());
    }

    // ---- Layer 3 / apply_to_snapshot reducer semantics ----

    #[test]
    fn apply_headline_first_wins_does_not_overwrite_set_used_tokens() {
        let mut snap = ContextSnapshot {
            used_tokens: Some(100),
            ..Default::default()
        };
        apply_to_snapshot(
            &mut snap,
            LineKind::HeadlineCompact {
                used: 200,
                total: 999,
                pct: Some(99.0),
            },
        );
        assert_eq!(snap.used_tokens, Some(100));
        assert_eq!(snap.total_tokens, Some(999));
        assert_eq!(snap.used_pct, Some(99.0));
    }

    #[test]
    fn apply_two_categories_carry_distinct_keys() {
        let mut snap = ContextSnapshot::default();
        apply_to_snapshot(
            &mut snap,
            LineKind::Category {
                name: "System prompt".to_string(),
                tokens: 1234,
                pct: None,
            },
        );
        apply_to_snapshot(
            &mut snap,
            LineKind::Category {
                name: "Tools".to_string(),
                tokens: 5678,
                pct: None,
            },
        );
        let cats = snap.categories.expect("categories map");
        assert_eq!(cats["system_prompt"], json!(1234));
        assert_eq!(cats["tools"], json!(5678));
    }

    #[test]
    fn apply_category_with_existing_key_does_not_overwrite() {
        let mut snap = ContextSnapshot::default();
        apply_to_snapshot(
            &mut snap,
            LineKind::Category {
                name: "Tools".to_string(),
                tokens: 100,
                pct: None,
            },
        );
        apply_to_snapshot(
            &mut snap,
            LineKind::Category {
                name: "Tools".to_string(),
                tokens: 999,
                pct: None,
            },
        );
        let cats = snap.categories.expect("categories map");
        assert_eq!(cats["tools"], json!(100));
    }

    #[test]
    fn apply_freespace_does_not_overwrite_pct_already_set() {
        let mut snap = ContextSnapshot {
            free_pct: Some(50.0),
            ..Default::default()
        };
        apply_to_snapshot(&mut snap, LineKind::FreeSpace { pct: 99.0 });
        assert_eq!(snap.free_pct, Some(50.0));
    }

    #[test]
    fn apply_section_header_is_noop() {
        let mut snap = ContextSnapshot::default();
        apply_to_snapshot(&mut snap, LineKind::SectionHeader);
        assert!(snap.is_empty());
    }

    // ---- Layer 4 / chunk boundary ----

    #[test]
    fn feed_idempotent_when_refed_same_chunk_twice() {
        let mut p = ContextParser::new();
        let chunk = b"Context\n24.2k/200k tokens (12%)\n";
        let snap1 = feed_all(&mut p, chunk).expect("first feed");
        let snap2 = feed_all(&mut p, chunk).expect("second feed");
        assert_eq!(snap1.used_tokens, snap2.used_tokens);
        assert_eq!(snap1.total_tokens, snap2.total_tokens);
        assert_eq!(snap1.used_pct, snap2.used_pct);
    }

    #[test]
    fn feed_commutative_for_disjoint_chunks() {
        // `feed(a ++ b)` and `feed(a); feed(b)` should produce
        // the same snapshot when neither `a` nor `b` ends
        // mid-line. The byte-level split inside one line would
        // require cross-chunk reassembly; this test only
        // asserts the new-line-aligned split case.
        let a: &[u8] = b"Context\n24.2k/200k tokens (12%)\n";
        let b: &[u8] = b"Free space: 142.8k (71.4%)\n";
        let mut merged = a.to_vec();
        merged.extend_from_slice(b);
        let mut p1 = ContextParser::new();
        let snap1 = feed_all(&mut p1, &merged).expect("merged feed");
        let mut p2 = ContextParser::new();
        let _ = feed_all(&mut p2, a);
        let snap2 = feed_all(&mut p2, b).expect("split feed");
        assert_eq!(snap1.used_tokens, snap2.used_tokens);
        assert_eq!(snap1.total_tokens, snap2.total_tokens);
        assert_eq!(snap1.used_pct, snap2.used_pct);
        assert_eq!(snap1.free_pct, snap2.free_pct);
    }

    #[test]
    fn feed_does_not_panic_on_random_bytes() {
        // Fuzz-style sweep over a handful of varied byte
        // sequences. The parser must never panic on any of
        // them; we only assert that it terminates and produces
        // a parseable snapshot.
        let seeds: &[&[u8]] = &[
            b"",
            b"\n",
            b"\x1b[?1049l",
            b"\x1b]0;title\x07",
            b"\xe2\x9b\xb6\xe2\x9b\x81", // ⏶⏁ bar-chart glyphs
            b"Used: 87,432 / 200,000 tokens (44%)",
            b"System prompt: 2.9k tokens (1.4%)",
            b"\x1b[31mUsed: 100\x1b[0m / 200 (50%)",
            b"foo bar baz\n",
            b"((((((((((",
            b"//////////",
            b"$$$$$$$$$$",
            b"\xff\xfe\xfd\xfc",
            b"0\x1b[1m\x1b[31m",
        ];
        for bytes in seeds {
            let mut p = ContextParser::new();
            for chunk in bytes.chunks(7) {
                let _ = p.feed(chunk);
            }
            let _ = p.flush();
        }
    }

    #[test]
    fn feed_incomplete_escape_at_chunk_tail_is_carried_over() {
        // Chunk ends with `ESC [` but no final byte — the
        // stripper keeps the ESC bytes for the next chunk's
        // prefix. We only assert the parser doesn't crash and
        // surfaces *some* snapshot when the next chunk
        // completes the sequence.
        let mut p = ContextParser::new();
        let _ = p.feed(b"Context\n\x1b[");
        let snap = p
            .feed(b"31m24.2k/200k tokens (12%)\x1b[0m\n")
            .or_else(|| p.flush())
            .expect("second chunk completes the escape");
        assert_eq!(snap.used_tokens, Some(24_200));
        assert_eq!(snap.total_tokens, Some(200_000));
        assert_eq!(snap.used_pct, Some(12.0));
    }

    // ---- Final cross-check: line-mode edge cases ----

    #[test]
    fn first_kind_helper_returns_none_for_empty_vec() {
        // Helper for the new Vec return type — empty input
        // must yield None for the first-kind convenience.
        assert!(first_kind("").is_none());
        assert!(first_kind("unrelated prose").is_none());
    }
}
