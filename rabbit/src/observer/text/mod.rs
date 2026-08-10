//!
//!
//! Anything that operates on a TUI byte stream before semantic
//! extraction lives here. Currently:
//!
//! - [`ansi::strip_ansi_bytes`] — deterministic ESC-sequence
//!   removal used by every modal parser that reads xterm-captured
//!   bytes.

pub mod ansi;
