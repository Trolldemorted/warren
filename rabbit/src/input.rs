use anyhow::Result;
use std::io::Write;

pub const ENTER: &[u8] = b"\r";
pub const ESC: &[u8] = b"\x1b";
pub const CTRL_U: &[u8] = b"\x15";

pub fn paste<W: Write>(w: &mut W, text: &str) -> Result<()> {
    for ch in text.chars() {
        if ch == '\n' {
            w.write_all(b"\r")?;
        } else {
            let mut buf = [0u8; 4];
            let s = ch.encode_utf8(&mut buf);
            w.write_all(s.as_bytes())?;
        }
    }
    w.write_all(ENTER)?;
    w.flush()?;
    Ok(())
}

pub fn slash<W: Write>(w: &mut W, cmd: &str) -> Result<()> {
    w.write_all(CTRL_U)?;
    let line = format!("/{cmd}");
    w.write_all(line.as_bytes())?;
    w.write_all(ENTER)?;
    w.flush()?;
    Ok(())
}

pub fn interrupt<W: Write>(w: &mut W) -> Result<()> {
    w.write_all(ESC)?;
    w.flush()?;
    Ok(())
}

#[allow(dead_code)]
pub fn raw<W: Write>(w: &mut W, bytes: &[u8]) -> Result<()> {
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}
