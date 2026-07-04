#[test]
fn openapi_documents_every_claude_path() {
    let yml = std::fs::read_to_string("openapi.yml").expect("read openapi.yml");
    let paths = [
        "/api/agents/{id}/claude/prompt",
        "/api/agents/{id}/claude/usage",
        "/api/agents/{id}/claude/state",
        "/api/agents/{id}/claude/clear",
        "/api/agents/{id}/claude/compact",
        "/api/agents/{id}/claude/interrupt",
        "/api/agents/{id}/claude/restart",
        "/api/agents/{id}/claude/events",
        "/api/agents/{id}/claude/events/stream",
    ];
    for p in paths {
        assert!(
            yml.contains(p),
            "openapi.yml is missing path {p} — keep router and docs in sync"
        );
    }
}

#[test]
fn openapi_yaml_is_parseable() {
    let yml = std::fs::read_to_string("openapi.yml").expect("read openapi.yml");
    let v: serde_yaml::Value = serde_yaml::from_str(&yml).expect("parse openapi.yml");
    assert!(v.get("paths").is_some());
}
