//! rabbit-hook: stdin → POST to the observer hook endpoint.
//!
//! Spawned by Claude as a hook handler; reads a JSON payload from stdin and
//! forwards it to the running rabbit's observer HTTP server (`/hook/:kind`).
//! Auto-discovered by Cargo from `src/bin/`.

use anyhow::Result;
use tokio::io::AsyncReadExt;

/// Refuse to forward an empty hook payload — observer-side handlers all
/// expect a non-empty `payload` (StopHook payloads carry JSON tool
/// results, PreToolUse payloads carry the tool name, etc.). An empty
/// stdin is almost always a misconfigured pipe (`rabbit-hook
/// </dev/null`) and silently dropping it would mask the bug. Exit
/// non-zero so claude's hook surface sees a failure and the operator
/// sees the error.
///
/// Extracted as a free function so the empty-input branch is unit-
/// testable without spawning a subprocess or standing up a fake HTTP
/// server. The trimming mirrors the live behavior at the call site.
fn validate_stdin_payload(payload: &str) -> Result<()> {
    if payload.trim().is_empty() {
        return Err(anyhow::anyhow!(
            "rabbit-hook: stdin is empty — provide JSON via pipe or redirect"
        ));
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    if let Err(e) = simple_logger::init_with_env() {
        eprintln!("rabbit-hook: failed to initialize logger: {e:?}");
    }

    let url = std::env::var("RABBIT_OBSERVER_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:7777".to_string());
    let kind = std::env::var("RABBIT_HOOK_KIND").unwrap_or_else(|_| "unknown".to_string());

    let mut stdin = tokio::io::stdin();
    let mut payload = String::new();
    stdin.read_to_string(&mut payload).await?;

    validate_stdin_payload(&payload)?;

    let body = serde_json::json!({
        "kind": kind,
        "payload": serde_json::from_str::<serde_json::Value>(&payload)
            .unwrap_or(serde_json::Value::String(payload)),
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let resp = client
        .post(format!("{url}/hook/{kind}"))
        .json(&body)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => Ok(()),
        Ok(r) => Err(anyhow::anyhow!("hook shim got status {}", r.status())),
        Err(e) => Err(anyhow::anyhow!("hook shim request failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::validate_stdin_payload;

    #[test]
    fn rejects_empty_stdin() {
        let err = validate_stdin_payload("").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("stdin is empty"),
            "expected 'stdin is empty' in error, got: {msg}"
        );
    }

    #[test]
    fn rejects_whitespace_only_stdin() {
        // Whitespace-only stdin is the typical `echo "" | rabbit-hook`
        // misconfiguration — the shell expands to a single newline, the
        // hook shim reads it, and without this guard we'd POST an
        // empty-string payload to the observer.
        for ws in ["   ", "\n", "\n\n", " \t \n "] {
            let err = validate_stdin_payload(ws).unwrap_err();
            assert!(
                err.to_string().contains("stdin is empty"),
                "expected rejection for whitespace input {ws:?}"
            );
        }
    }

    #[test]
    fn accepts_real_payload() {
        validate_stdin_payload(r#"{"stop_reason":"end_turn"}"#)
            .expect("non-empty payload must validate");
    }

    #[test]
    fn accepts_payload_with_leading_whitespace() {
        // A real StopHook payload delivered through Claude's pipe
        // typically has a trailing newline. As long as there's at
        // least one non-whitespace character, we accept it.
        validate_stdin_payload("\n{\"k\":\"v\"}\n")
            .expect("non-empty payload with surrounding whitespace must validate");
    }
}
