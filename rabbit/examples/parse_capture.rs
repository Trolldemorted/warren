//! Replay a captured PTY byte stream through the production `/context`
//! parser and print the resulting [`ContextSnapshot`].
//!
//! Useful when a parser bug only surfaces against real interactive
//! Claude output and you don't want to stand up the full
//! warren ↔ rabbit stack to reproduce it. Capture a session with
//! the helper script in the project root (e.g. `python3 /tmp/cap2.py
//! /tmp/context.bin`) and feed the bytes through here.
//!
//! Run with:
//!
//! ```sh
//! cargo run -p rabbit --example parse_capture -- /tmp/context.bin
//! ```
//!
//! Defaults to `/tmp/context4.bin` (the 8715-byte known-good
//! capture) when no path is given. The file is fed in 256-byte
//! chunks to match the production PTY reader's write size so
//! cross-chunk boundary bugs (line_buf carry-over, partial
//! `Used:` across chunks, OSC+ST terminator leakage) reproduce
//! faithfully.

use rabbit::observer::context::ContextParser;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/context4.bin".to_string());
    let bytes = std::fs::read(&path).unwrap_or_else(|err| {
        eprintln!("parse_capture: cannot read {}: {}", path, err);
        eprintln!("usage: cargo run -p rabbit --example parse_capture -- <capture.bin>");
        std::process::exit(2);
    });
    let mut p = ContextParser::new();
    for chunk in bytes.chunks(256) {
        let _ = p.feed(chunk);
    }
    let snap = p.flush().expect("flush emits partial state");
    println!("used_tokens: {:?}", snap.used_tokens);
    println!("total_tokens: {:?}", snap.total_tokens);
    println!("used_pct: {:?}", snap.used_pct);
    println!("free_pct: {:?}", snap.free_pct);
    println!("window_tokens: {:?}", snap.window_tokens);
    println!(
        "categories: {}",
        serde_json::to_string_pretty(&snap.categories).unwrap_or_else(|_| "None".into())
    );
}
