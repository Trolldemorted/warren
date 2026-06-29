use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::process::ExitCode;

fn is_admin() -> bool {
    std::env::var("WARREN_ADMIN").ok().as_deref() == Some("1")
}

fn build_requests_cmd(admin: bool) -> Command {
    let mut c = Command::new("requests").about(if admin {
        "List, create, delete, claim, respond to, ack, or approve/reject requests"
    } else {
        "List, create, claim, respond to, or acknowledge requests"
    });
    c = c
        .subcommand(
            Command::new("list")
                .about("List requests")
                .arg(Arg::new("status").long("status").num_args(1).value_parser([
                    "awaiting_admin_request_approval",
                    "awaiting_agent_request_claim",
                    "awaiting_agent_response",
                    "awaiting_admin_response_approval",
                    "awaiting_agent_response_acknowledge",
                    "done",
                    "rejected",
                ]))
                .arg(
                    Arg::new("all")
                        .long("all")
                        .help("Include done rows in the listing")
                        .action(ArgAction::SetTrue),
                ),
        )
        .subcommand(
            Command::new("create")
                .about("Create a request")
                .arg(Arg::new("class").long("class").num_args(1).required(true))
                .arg(Arg::new("kind").long("type").num_args(1).value_name("KIND"))
                .arg(
                    Arg::new("file")
                        .short('f')
                        .long("file")
                        .num_args(1)
                        .conflicts_with("payload"),
                )
                .arg(
                    Arg::new("payload")
                        .short('p')
                        .long("payload")
                        .num_args(1)
                        .conflicts_with("file"),
                )
                .arg(
                    Arg::new("approve")
                        .long("approve")
                        .action(ArgAction::SetTrue),
                )
                .arg(
                    Arg::new("channel")
                        .long("channel")
                        .num_args(1)
                        .value_name("CHANNEL_ID"),
                ),
        )
        .subcommand(
            Command::new("delete")
                .about("Delete a request (admin)")
                .hide(!admin)
                .arg(Arg::new("id").num_args(1).required(true)),
        )
        .subcommand(
            Command::new("claim")
                .about("Atomically claim a request")
                .arg(Arg::new("id").num_args(1).required(true)),
        )
        .subcommand(
            Command::new("respond")
                .about("Respond to a request you previously claimed")
                .arg(Arg::new("id").num_args(1).required(true))
                .arg(
                    Arg::new("file")
                        .short('f')
                        .long("file")
                        .num_args(1)
                        .conflicts_with("payload"),
                )
                .arg(
                    Arg::new("payload")
                        .short('p')
                        .long("payload")
                        .num_args(1)
                        .conflicts_with("file"),
                ),
        )
        .subcommand(
            Command::new("acknowledge")
                .about("Mark an accepted response as consumed (status 4 → 5)")
                .arg(Arg::new("id").num_args(1).required(true)),
        )
        .subcommand(
            Command::new("unacknowledge")
                .about("Revert a done request back to awaiting agent acknowledgement (admin)")
                .hide(!admin)
                .arg(Arg::new("id").num_args(1).required(true)),
        )
        .subcommand(
            Command::new("approve")
                .about("Approve a pending request (admin)")
                .hide(!admin)
                .arg(Arg::new("id").num_args(1).required(true)),
        )
        .subcommand(
            Command::new("reject")
                .about("Reject a pending request (admin)")
                .hide(!admin)
                .arg(Arg::new("id").num_args(1).required(true)),
        )
        .subcommand(
            Command::new("accept-response")
                .about("Accept a response (admin)")
                .hide(!admin)
                .arg(Arg::new("id").num_args(1).required(true)),
        )
        .subcommand(
            Command::new("reject-response")
                .about("Reject a response (admin)")
                .hide(!admin)
                .arg(Arg::new("id").num_args(1).required(true)),
        );
    c
}

fn build_agents_cmd(admin: bool) -> Command {
    Command::new("agents")
        .about("List, create, or delete agents (admin)")
        .subcommand(Command::new("list").about("List agents"))
        .subcommand(
            Command::new("create")
                .about("Create an agent")
                .arg(Arg::new("name").long("name").num_args(1).required(true))
                .arg(Arg::new("class").long("class").num_args(1).required(true))
                .arg(Arg::new("kind").long("kind").num_args(1))
                .arg(Arg::new("model").long("model").num_args(1).required(true)),
        )
        .subcommand(
            Command::new("delete")
                .about("Delete an agent")
                .arg(Arg::new("id").num_args(1).required(true)),
        )
        .hide(!admin)
}

fn build_channels_cmd(admin: bool) -> Command {
    Command::new("channels")
        .about("List, create, or delete channels (admin)")
        .subcommand(Command::new("list").about("List channels"))
        .subcommand(
            Command::new("create")
                .about("Create a channel")
                .arg(
                    Arg::new("sender_class")
                        .long("sender-class")
                        .num_args(1)
                        .required(true),
                )
                .arg(Arg::new("sender_kind").long("sender-kind").num_args(1))
                .arg(
                    Arg::new("receiver_class")
                        .long("receiver-class")
                        .num_args(1)
                        .required(true),
                )
                .arg(Arg::new("receiver_kind").long("receiver-kind").num_args(1))
                .arg(
                    Arg::new("description")
                        .long("description")
                        .num_args(1)
                        .required(true),
                ),
        )
        .subcommand(
            Command::new("delete")
                .about("Delete a channel")
                .arg(Arg::new("id").num_args(1).required(true)),
        )
        .hide(!admin)
}

fn build_cli() -> Command {
    let admin = is_admin();
    Command::new("warren-cli")
        .about("warren admin + agent CLI")
        .version(env!("CARGO_PKG_VERSION"))
        .arg(
            Arg::new("url")
                .long("url")
                .env("WARREN_URL")
                .num_args(1)
                .required(true),
        )
        .arg(
            Arg::new("token")
                .long("token")
                .env("WARREN_TOKEN")
                .num_args(1)
                .required(true)
                .help("Admin session token (from /api/login) OR agent authtoken"),
        )
        .subcommand(build_requests_cmd(admin))
        .subcommand(build_agents_cmd(admin))
        .subcommand(build_channels_cmd(admin))
}

fn main() -> ExitCode {
    let cli = build_cli().get_matches();
    let agent = ureq::AgentBuilder::new().build();

    let res = run(&cli, &agent);
    match res {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_cmd_help(mut cmd: Command) -> Result<String, String> {
    let mut buf = Vec::new();
    cmd.write_help(&mut buf).map_err(|e| format!("{e}"))?;
    let s = String::from_utf8(buf).map_err(|e| format!("{e}"))?;
    let _ = std::io::stdout().write_all(s.as_bytes());
    Ok(String::new())
}

fn run(cli: &ArgMatches, agent: &ureq::Agent) -> Result<String, String> {
    let url = cli.get_one::<String>("url").unwrap().clone();
    let token = cli.get_one::<String>("token").unwrap().clone();
    let admin = is_admin();

    match cli.subcommand() {
        None => {
            let body = http_get(agent, &url, &token, "/api/agents/me")?;
            Ok(strip_authtoken(&body))
        }

        Some(("agents", m)) => match m.subcommand() {
            Some(("list", _)) => http_get(agent, &url, &token, "/api/agents"),
            Some(("create", sc)) => {
                let body = serde_json::json!({
                    "name": sc.get_one::<String>("name").unwrap(),
                    "class": sc.get_one::<String>("class").unwrap(),
                    "kind": sc.get_one::<String>("kind").map(String::as_str),
                    "model": sc.get_one::<String>("model").unwrap(),
                });
                http_post(agent, &url, &token, "/api/agents", &body.to_string())
            }
            Some(("delete", sc)) => {
                let id = sc.get_one::<String>("id").unwrap();
                http_delete(agent, &url, &token, &format!("/api/agents/{id}"))
            }
            _ => print_cmd_help(build_agents_cmd(admin)),
        },

        Some(("requests", m)) => match m.subcommand() {
            Some(("list", sc)) => {
                let mut q: Vec<String> = Vec::new();
                if let Some(s) = sc.get_one::<String>("status") {
                    q.push(format!("status={}", urlencode(s)));
                }
                if sc.get_flag("all") {
                    q.push("include_done=true".to_string());
                }
                let qs = if q.is_empty() {
                    String::new()
                } else {
                    format!("?{}", q.join("&"))
                };
                http_get(agent, &url, &token, &format!("/api/requests{qs}"))
            }
            Some(("create", sc)) => {
                let payload = read_payload(
                    sc.get_one::<String>("file"),
                    sc.get_one::<String>("payload"),
                );
                let body = serde_json::json!({
                    "target_class": sc.get_one::<String>("class").unwrap(),
                    "target_type": sc.get_one::<String>("kind").map(String::as_str),
                    "payload": payload,
                    "approved": sc.get_flag("approve"),
                    "channel_id": sc.get_one::<String>("channel").map(String::as_str),
                });
                http_post(agent, &url, &token, "/api/requests", &body.to_string())
            }
            Some(("delete", sc)) => {
                let id = sc.get_one::<String>("id").unwrap();
                http_delete(agent, &url, &token, &format!("/api/requests/{id}"))
            }
            Some(("approve", sc)) => {
                let id = sc.get_one::<String>("id").unwrap();
                http_post(
                    agent,
                    &url,
                    &token,
                    &format!("/api/requests/{id}/approve"),
                    "",
                )
            }
            Some(("reject", sc)) => {
                let id = sc.get_one::<String>("id").unwrap();
                http_post(
                    agent,
                    &url,
                    &token,
                    &format!("/api/requests/{id}/reject"),
                    "",
                )
            }
            Some(("accept-response", sc)) => {
                let id = sc.get_one::<String>("id").unwrap();
                http_post(
                    agent,
                    &url,
                    &token,
                    &format!("/api/requests/{id}/accept-response"),
                    "",
                )
            }
            Some(("reject-response", sc)) => {
                let id = sc.get_one::<String>("id").unwrap();
                http_post(
                    agent,
                    &url,
                    &token,
                    &format!("/api/requests/{id}/reject-response"),
                    "",
                )
            }
            Some(("claim", sc)) => {
                let id = sc.get_one::<String>("id").unwrap();
                http_post(
                    agent,
                    &url,
                    &token,
                    &format!("/api/requests/{id}/claim"),
                    "",
                )
            }
            Some(("respond", sc)) => {
                let id = sc.get_one::<String>("id").unwrap();
                let payload = read_payload(
                    sc.get_one::<String>("file"),
                    sc.get_one::<String>("payload"),
                );
                let body = serde_json::json!({ "response": payload }).to_string();
                http_post(
                    agent,
                    &url,
                    &token,
                    &format!("/api/requests/{id}/respond"),
                    &body,
                )
            }
            Some(("acknowledge", sc)) => {
                let id = sc.get_one::<String>("id").unwrap();
                http_post(
                    agent,
                    &url,
                    &token,
                    &format!("/api/requests/{id}/acknowledge"),
                    "",
                )
            }
            Some(("unacknowledge", sc)) => {
                let id = sc.get_one::<String>("id").unwrap();
                http_post(
                    agent,
                    &url,
                    &token,
                    &format!("/api/requests/{id}/unacknowledge"),
                    "",
                )
            }
            _ => print_cmd_help(build_requests_cmd(admin)),
        },

        Some(("channels", m)) => match m.subcommand() {
            Some(("list", _)) => http_get(agent, &url, &token, "/api/channels"),
            Some(("create", sc)) => {
                let body = serde_json::json!({
                    "sender_class": sc.get_one::<String>("sender_class").unwrap(),
                    "sender_kind": sc.get_one::<String>("sender_kind").map(String::as_str),
                    "receiver_class": sc.get_one::<String>("receiver_class").unwrap(),
                    "receiver_kind": sc.get_one::<String>("receiver_kind").map(String::as_str),
                    "description": sc.get_one::<String>("description").unwrap(),
                });
                http_post(agent, &url, &token, "/api/channels", &body.to_string())
            }
            Some(("delete", sc)) => {
                let id = sc.get_one::<String>("id").unwrap();
                http_delete(agent, &url, &token, &format!("/api/channels/{id}"))
            }
            _ => print_cmd_help(build_channels_cmd(admin)),
        },

        _ => Err("unknown subcommand".to_string()),
    }
}

fn read_payload(file: Option<&String>, payload: Option<&String>) -> String {
    match (file, payload) {
        (Some(p), _) => match fs::read_to_string(p) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("read {p}: {e}");
                String::new()
            }
        },
        (None, Some(s)) => s.clone(),
        (None, None) => String::new(),
    }
}

fn strip_authtoken(body: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    if let Some(agent) = v.get_mut("agent").and_then(|a| a.as_object_mut()) {
        agent.remove("authtoken");
    }
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| body.to_string())
}

fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

fn http_get(agent: &ureq::Agent, base: &str, token: &str, path: &str) -> Result<String, String> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    agent
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| format!("{e}"))
        .and_then(|r| r.into_string().map_err(|e| format!("read body: {e}")))
}

fn http_post(
    agent: &ureq::Agent,
    base: &str,
    token: &str,
    path: &str,
    body: &str,
) -> Result<String, String> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let mut rb = agent
        .post(&url)
        .set("Authorization", &format!("Bearer {token}"));
    if !body.is_empty() {
        rb = rb.set("Content-Type", "application/json");
    }
    rb.send_string(body)
        .map_err(|e| format!("{e}"))
        .and_then(|r| r.into_string().map_err(|e| format!("read body: {e}")))
}

fn http_delete(agent: &ureq::Agent, base: &str, token: &str, path: &str) -> Result<String, String> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    agent
        .delete(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| format!("{e}"))
        .and_then(|r| r.into_string().map_err(|e| format!("read body: {e}")))
}
