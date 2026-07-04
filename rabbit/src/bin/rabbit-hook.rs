//! rabbit-hook: stdin → POST to the observer hook endpoint.
//!
//! Spawned by Claude as a hook handler; reads a JSON payload from stdin and
//! forwards it to the running rabbit's observer HTTP server (`/hook/:kind`).
//! Auto-discovered by Cargo from `src/bin/`.

use anyhow::Result;
use tokio::io::AsyncReadExt;

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