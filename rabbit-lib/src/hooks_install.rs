use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const HOOK_KINDS: &[(&str, &str)] = &[
    ("SessionStart", "session_start"),
    ("UserPromptSubmit", "user_prompt_submit"),
    ("Stop", "stop"),
    ("SessionEnd", "session_end"),
];

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
    for (claude_key, kind) in HOOK_KINDS {
        let entry = json!([{
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": hook_bin.to_string_lossy(),
                "env": { "RABBIT_HOOK_KIND": kind },
            }],
        }]);
        hooks.insert((*claude_key).to_string(), entry);
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
    use std::collections::HashMap;
    use tempfile::tempdir;

    #[test]
    fn overwrites_with_four_hooks_when_absent() {
        let dir = tempdir().unwrap();
        let bin = PathBuf::from("/usr/local/bin/rabbit-hook");
        install(dir.path(), &bin).unwrap();
        let raw = std::fs::read_to_string(dir.path().join(".claude/settings.json")).unwrap();
        let v: Value = serde_json::from_str(&raw).unwrap();
        let hooks = v.get("hooks").expect("hooks key");
        for (claude_key, kind) in HOOK_KINDS {
            let arr = hooks
                .get(claude_key)
                .unwrap_or_else(|| panic!("{claude_key}"));
            assert!(arr.is_array(), "{claude_key} should be array");
            let inner = &arr[0]["hooks"][0];
            assert_eq!(inner["command"], "/usr/local/bin/rabbit-hook");
            assert_eq!(inner["env"]["RABBIT_HOOK_KIND"], *kind);
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
    fn hook_env_has_no_duplicate_keys() {
        let out = build(Path::new("rabbit-hook"));
        let mut seen: HashMap<String, String> = HashMap::new();
        for (claude_key, expected) in HOOK_KINDS {
            let v: Value = serde_json::from_str(&out).unwrap();
            let kind = v["hooks"][*claude_key][0]["hooks"][0]["env"]["RABBIT_HOOK_KIND"]
                .as_str()
                .unwrap()
                .to_string();
            let prev = seen.insert(claude_key.to_string(), kind.clone());
            assert!(prev.is_none(), "duplicate key {claude_key}");
            assert_eq!(&kind, expected);
        }
    }
}
