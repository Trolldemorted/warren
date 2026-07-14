// Capture the raw bytes Claude Code's `/context` modal emits when
// it's running in a real PTY. Output is written to a path supplied
// via argv[1] as a binary blob. The harness:
//
// 1. Spawns `claude --dangerously-skip-permissions` in a PTY at
//    120×40 (the production TUI size).
// 2. Drains the PTY reader into an in-memory buffer.
// 3. Auto-accepts the trust dialog by watching for the marker and
//    sending `\r` once.
// 4. Waits for the prompt input to be ready (the `❯` glyph on the
//    input row), then writes `/context` followed by Enter.
// 5. Records bytes for 1500 ms (longer than the production 700 ms
//    budget so we capture the full paint).
// 6. Writes the captured bytes to argv[1] and exits.
//
// This binary is intentionally NOT part of the test suite — it
// requires a live `claude` on PATH and is only run by a developer
// to refresh the `/context` fixture (`/context_modal.bin`) when
// Claude Code's modal format changes.
use anyhow::{Context, Result};
use rabbit::pty::Pty;
use rabbit::trust::TrustWatcher;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    let out_path = std::env::args()
        .nth(1)
        .context("usage: capture-context-modal <out.bin>")?;
    let workdir = std::env::var("CAPTURE_WORKDIR").unwrap_or_else(|_| "/tmp".to_string());

    let mut pty = Pty::spawn(
        "claude",
        &["--dangerously-skip-permissions".to_string()],
        &workdir,
        120,
        40,
        0,
    )
    .context("spawning claude")?;

    let mut reader = pty.reader();
    let mut writer = pty.writer();

    // Trust dialog auto-accept.
    let mut watcher = TrustWatcher::new(2);
    let mut all_bytes: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let start = Instant::now();
    let trust_deadline = start + Duration::from_secs(15);
    let mut accepted = false;

    while Instant::now() < trust_deadline {
        if !pty.alive() {
            anyhow::bail!("claude exited before trust dialog");
        }
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                all_bytes.extend_from_slice(chunk);
                if !accepted {
                    if let Some(accept) = watcher.observe(chunk) {
                        writer.write_all(accept).context("writing accept")?;
                        writer.flush().ok();
                        accepted = true;
                        eprintln!("[capture] trust dialog accepted");
                        break;
                    }
                }
            }
        }
    }

    if !accepted {
        anyhow::bail!("timed out waiting for trust dialog");
    }

    // Give claude a moment to settle on the prompt input row.
    std::thread::sleep(Duration::from_millis(500));

    // Drain any post-accept output so it doesn't pollute the capture.
    let settle_deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < settle_deadline {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                all_bytes.extend_from_slice(&buf[..n]);
            }
        }
    }

    // Send `/context` exactly as `input::slash` would.
    eprintln!("[capture] sending /context");
    writer
        .write_all(b"\x15/context\r")
        .context("writing /context")?;
    writer.flush().ok();

    // Record bytes for 1500 ms — long enough to capture the full
    // modal paint plus any post-paint re-render.
    let capture_deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < capture_deadline {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                all_bytes.extend_from_slice(chunk);
                // Sanity log so the developer can see it's working.
                let preview: String = chunk
                    .iter()
                    .take(80)
                    .filter(|b| b.is_ascii_graphic() || **b == b' ' || **b == b'\n')
                    .map(|b| *b as char)
                    .collect();
                eprintln!("[capture] +{} bytes: {:?}", n, preview);
            }
        }
    }

    drop(writer);
    let _ = pty.terminate();
    let _ = pty.wait();

    std::fs::write(&out_path, &all_bytes)
        .with_context(|| format!("writing capture to {out_path}"))?;
    eprintln!("[capture] wrote {} bytes to {}", all_bytes.len(), out_path);

    // Sanity: the capture must contain at least one byte that looks
    // like a `/context` header or `Used:` line so we know we caught
    // the modal (not the trust dialog or the prompt echo).
    let text = String::from_utf8_lossy(&all_bytes);
    let lower = text.to_ascii_lowercase();
    if !lower.contains("used") && !lower.contains("/context") && !lower.contains("context") {
        anyhow::bail!(
            "capture does not contain any recognizable /context markers; \
             got text snippet: {:?}",
            &lower.chars().take(400).collect::<String>()
        );
    }
    Ok(())
}

#[allow(unused_imports)]
use rabbit;
