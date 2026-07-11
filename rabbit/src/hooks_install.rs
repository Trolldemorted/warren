use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Claude lifecycle events rabbit installs a hook for. The event name is
/// the settings.json key; the shared `rabbit-hook` binary discovers which
/// one fired from the `hook_event_name` field on its stdin payload (Claude
/// Code silently ignores any `env` block, so it can't be used to tag the
/// event). No `matcher` is emitted — it's ignored for `UserPromptSubmit` /
/// `Stop`, and an empty matcher would stop `SessionStart` / `SessionEnd`
/// from ever firing.
const HOOK_EVENTS: &[&str] = &["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"];

pub fn install(workdir: &Path, hook_bin: &Path) -> Result<()> {
    let dir = workdir.join(".claude");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("settings.json");
    let body = build(hook_bin);
    write_atomic(&path, &body).with_context(|| format!("writing {}", path.display()))?;
    log::info!("wrote claude hook settings to {}", path.display());
    Ok(())
}

fn build(hook_bin: &Path) -> String {
    let mut settings = json!({});
    let mut hooks = serde_json::Map::new();
    for event in HOOK_EVENTS {
        let entry = json!([{
            "hooks": [{
                "type": "command",
                "command": hook_bin.to_string_lossy(),
            }],
        }]);
        hooks.insert((*event).to_string(), entry);
    }
    settings["hooks"] = Value::Object(hooks);
    serde_json::to_string_pretty(&settings).expect("static json")
}

fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

pub fn resolve_hook_bin(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    std::env::current_exe()
        .ok()
        .map(|p| p.with_file_name("rabbit-hook"))
        .unwrap_or_else(|| PathBuf::from("rabbit-hook"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn overwrites_with_four_hooks_when_absent() {
        let dir = tempdir().unwrap();
        let bin = PathBuf::from("/usr/local/bin/rabbit-hook");
        install(dir.path(), &bin).unwrap();
        let raw = std::fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        let hooks = v.get("hooks").expect("hooks key");
        for event in HOOK_EVENTS {
            let arr = hooks.get(event).unwrap_or_else(|| panic!("{event}"));
            assert!(arr.is_array(), "{event} should be array");
            let inner = &arr[0]["hooks"][0];
            assert_eq!(inner["command"], "/usr/local/bin/rabbit-hook");
            // Claude Code ignores an `env` block and an empty `matcher`;
            // rabbit-hook reads the event from `hook_event_name` on stdin,
            // so neither field is emitted.
            assert!(inner.get("env").is_none(), "{event} must omit env");
            assert!(arr[0].get("matcher").is_none(), "{event} must omit matcher");
        }
    }

    #[test]
    fn overwrites_when_present_with_unrelated_keys() {
        let dir = tempdir().unwrap();
        let bin = PathBuf::from("rabbit-hook");
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(
            dir.path().join(".claude/settings.json"),
            serde_json::to_string_pretty(&json!({
                "permissions": { "allow": ["x"] },
                "mcpServers": { "foo": {} },
            }))
            .unwrap(),
        )
        .unwrap();
        install(dir.path(), &bin).unwrap();
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap(),
        )
        .unwrap();
        assert!(v.get("hooks").is_some(), "hooks must be present");
        assert!(
            v.get("permissions").is_none() && v.get("mcpServers").is_none(),
            "overwrite policy drops unrelated keys"
        );
    }

    #[test]
    fn overwrite_is_idempotent() {
        let dir = tempdir().unwrap();
        let bin = PathBuf::from("rabbit-hook");
        install(dir.path(), &bin).unwrap();
        let first = std::fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        install(dir.path(), &bin).unwrap();
        let second = std::fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn error_on_unwritable_parent_dir() {
        // A path whose first component does not exist and which sits at a
        // location unprivileged tests cannot write into (e.g. a missing path
        // under a directory we don't own). On Linux we can rely on the well-
        // known unwritable root "/proc/cmdline/-" which is not a directory.
        let bad = PathBuf::from("/proc/cmdline/nope");
        let res = install(&bad, Path::new("rabbit-hook"));
        assert!(res.is_err(), "expected error for {bad:?}");
    }

    #[test]
    fn resolve_hook_bin_uses_current_exe_when_unset() {
        let p = resolve_hook_bin(None);
        assert!(p.ends_with("rabbit-hook"), "got {p:?}");
    }

    #[test]
    fn resolve_hook_bin_prefers_explicit() {
        let explicit = PathBuf::from("/opt/custom/hook-cli");
        assert_eq!(resolve_hook_bin(Some(explicit.clone())), explicit);
    }

    #[test]
    fn command_path_is_a_string_not_binary() {
        let bin = PathBuf::from("/usr/local/bin/rabbit-hook");
        let out = build(&bin);
        let v: Value = serde_json::from_str(&out).unwrap();
        let cmd = &v["hooks"]["SessionStart"][0]["hooks"][0]["command"];
        assert!(cmd.is_string(), "command must be a JSON string, got {cmd}");
    }

    #[test]
    fn each_event_registered_once_without_matcher_or_env() {
        let out = build(Path::new("rabbit-hook"));
        let v: Value = serde_json::from_str(&out).unwrap();
        let hooks = v["hooks"].as_object().expect("hooks object");
        assert_eq!(hooks.len(), HOOK_EVENTS.len(), "one entry per event");
        for event in HOOK_EVENTS {
            let entry = &v["hooks"][*event];
            assert!(entry.is_array(), "{event} should be array");
            assert!(
                entry[0].get("matcher").is_none(),
                "{event} must omit matcher (empty matcher blocks session events)"
            );
            assert!(
                entry[0]["hooks"][0].get("env").is_none(),
                "{event} must omit the ignored env block"
            );
        }
    }
}
