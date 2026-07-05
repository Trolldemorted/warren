use anyhow::Result;
use std::io::Write;

pub const ENTER: &[u8] = b"\r";
pub const ESC: &[u8] = b"\x1b";
pub const CTRL_U: &[u8] = b"\x15";
/// Ctrl-C byte (`0x03` / ETX). Used to forward a real Ctrl-C
/// keystroke into claude — distinct from `ESC`, which is what the
/// `EnvelopeBody::Interrupt` UI button sends (claude's "resuming the
/// full session will consume a substantial portion of your usage limits"
/// choice reacts to ESC, not Ctrl-C).
///
/// Resolved to the literal byte so the SIGINT-terminal-passthrough path
/// (when the user presses Ctrl+C in the terminal where rabbit runs)
/// can write the same byte claude would have received if rabbit were
/// not in front of it.
pub const CTRL_C: &[u8] = b"\x03";
/// Bracketed-paste start marker (`ESC[200~`). When the receiver is in
/// bracketed-paste mode, everything between START and END lands as a single
/// paste event instead of being interpreted keystroke-by-keystroke. This is
/// the §A.2 input-discipline rule for `prompt(text)` and is what stops
/// multi-line prompts from submitting at the first newline.
pub const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
/// Bracketed-paste end marker (`ESC[201~`).
pub const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

/// §A.2 programmatic prompt submission.
///
/// Sequence: `Ctrl-U` (clear the input line) + bracketed-paste open + text
/// (with `\n` translated to `\r`) + bracketed-paste close + `\r` (Enter).
/// The Ctrl-U prefix is the §A.2 safety rule for queueing a prompt while the
/// meta plane says Idle — it guarantees the new paste lands on an empty line
/// even if a prior keystroke left a partial fragment behind.
///
/// We do not escape any embedded `\x1b[201~` inside the text. A paste whose
/// body contains the literal end marker would prematurely close the paste;
/// callers (rabbit's wire path, warren's prompt API) reject such input at
/// validation time rather than papering over it here.
pub fn paste<W: Write + ?Sized>(w: &mut W, text: &str) -> Result<()> {
    w.write_all(CTRL_U)?;
    w.write_all(BRACKETED_PASTE_START)?;
    for ch in text.chars() {
        if ch == '\n' {
            w.write_all(b"\r")?;
        } else {
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            w.write_all(s.as_bytes())?;
        }
    }
    w.write_all(BRACKETED_PASTE_END)?;
    w.write_all(ENTER)?;
    w.flush()?;
    Ok(())
}

pub fn slash<W: Write + ?Sized>(w: &mut W, cmd: &str) -> Result<()> {
    w.write_all(CTRL_U)?;
    let line = format!("/{cmd}");
    w.write_all(line.as_bytes())?;
    w.write_all(ENTER)?;
    w.flush()?;
    Ok(())
}

pub fn interrupt<W: Write + ?Sized>(w: &mut W) -> Result<()> {
    w.write_all(ESC)?;
    w.flush()?;
    Ok(())
}

#[allow(dead_code)]
pub fn raw<W: Write + ?Sized>(w: &mut W, bytes: &[u8]) -> Result<()> {
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(prompt: &str) -> Vec<u8> {
        let mut out = Vec::new();
        paste(&mut out, prompt).unwrap();
        out
    }

    #[test]
    fn paste_emits_bracketed_sequence_with_ctrl_u_prefix_and_enter_suffix() {
        let out = capture("hello");
        assert_eq!(
            out,
            b"\x15\x1b[200~hello\x1b[201~\r".to_vec(),
            "Ctrl-U + START + text + END + ENTER"
        );
    }

    #[test]
    fn paste_translates_newlines_to_cr_inside_paste() {
        let out = capture("line1\nline2\n");
        assert_eq!(
            out,
            b"\x15\x1b[200~line1\rline2\r\x1b[201~\r".to_vec(),
            "newlines become \\r inside the bracketed paste, ENTER stays \\r"
        );
    }

    #[test]
    fn paste_preserves_unicode_chars() {
        let out = capture("héllo 🐇");
        let s = std::str::from_utf8(&out).expect("utf-8");
        assert!(s.starts_with("\x15\x1b[200~"));
        assert!(s.ends_with("\x1b[201~\r"));
        assert!(s.contains("héllo 🐇"));
    }

    #[test]
    fn slash_writes_ctrl_u_then_cmd_then_enter() {
        let mut out = Vec::new();
        slash(&mut out, "usage").unwrap();
        assert_eq!(out, b"\x15/usage\r".to_vec());
    }

    #[test]
    fn interrupt_writes_esc() {
        let mut out = Vec::new();
        interrupt(&mut out).unwrap();
        assert_eq!(out, b"\x1b".to_vec());
    }
}
