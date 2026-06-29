use clap::{Parser, Subcommand};
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(name = "warren-cli", about = "warren admin + agent CLI", version)]
struct Cli {
    #[arg(long, env = "WARREN_URL")]
    url: String,

    #[arg(
        long,
        env = "WARREN_TOKEN",
        help = "Admin session token (from /api/login) OR agent authtoken"
    )]
    token: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Show the current authenticated principal (uses /api/agents/me).
    Whoami,

    /// List, create, or delete agents.
    #[command(subcommand)]
    Agents(AgentsCmd),

    /// List, create, approve, or reject requests.
    #[command(subcommand)]
    Requests(RequestsCmd),

    /// Agent inbox: list requests sent by, claimable by, claimed by, or
    /// responded by you.
    InboxRequests,
    /// Atomically claim a request.
    Claim { id: String },
    /// Respond to a request you previously claimed.
    Respond {
        id: String,
        #[arg(short, long, conflicts_with = "payload")]
        file: Option<PathBuf>,
        #[arg(short, long, conflicts_with = "file")]
        payload: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum AgentsCmd {
    List,
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        class: String,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        model: String,
    },
    Delete {
        id: String,
    },
}

#[derive(Subcommand, Debug)]
enum RequestsCmd {
    List {
        #[arg(long)]
        status: Option<String>,
    },
    Create {
        #[arg(long)]
        class: String,
        #[arg(long = "type")]
        kind: Option<String>,
        #[arg(short, long, conflicts_with = "payload")]
        file: Option<PathBuf>,
        #[arg(short, long, conflicts_with = "file")]
        payload: Option<String>,
        #[arg(long)]
        approve: bool,
    },
    Approve {
        id: String,
    },
    Reject {
        id: String,
    },
    AcceptResponse {
        id: String,
    },
    RejectResponse {
        id: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
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

fn run(cli: &Cli, agent: &ureq::Agent) -> Result<String, String> {
    match &cli.cmd {
        Cmd::Whoami => cli.get(agent, "/api/agents/me"),

        Cmd::Agents(AgentsCmd::List) => cli.get(agent, "/api/agents"),
        Cmd::Agents(AgentsCmd::Create {
            name,
            class,
            kind,
            model,
        }) => {
            let body = serde_json::json!({
                "name": name,
                "class": class,
                "kind": kind,
                "model": model,
            });
            cli.post(agent, "/api/agents", &body.to_string())
        }
        Cmd::Agents(AgentsCmd::Delete { id }) => cli.delete(agent, &format!("/api/agents/{id}")),

        Cmd::Requests(RequestsCmd::List { status }) => {
            let q = status
                .as_deref()
                .map(|s| format!("?status={}", urlencode(s)))
                .unwrap_or_default();
            cli.get(agent, &format!("/api/requests{q}"))
        }
        Cmd::Requests(RequestsCmd::Create {
            class,
            kind,
            file,
            payload,
            approve,
        }) => {
            let payload = read_payload(file.as_deref(), payload.as_deref());
            let body = serde_json::json!({
                "target_class": class,
                "target_type": kind,
                "payload": payload,
                "approved": approve,
            });
            cli.post(agent, "/api/requests", &body.to_string())
        }
        Cmd::Requests(RequestsCmd::Approve { id }) => {
            cli.post(agent, &format!("/api/requests/{id}/approve"), "")
        }
        Cmd::Requests(RequestsCmd::Reject { id }) => {
            cli.post(agent, &format!("/api/requests/{id}/reject"), "")
        }
        Cmd::Requests(RequestsCmd::AcceptResponse { id }) => {
            cli.post(agent, &format!("/api/requests/{id}/accept-response"), "")
        }
        Cmd::Requests(RequestsCmd::RejectResponse { id }) => {
            cli.post(agent, &format!("/api/requests/{id}/reject-response"), "")
        }

        Cmd::InboxRequests => cli.get(agent, "/api/requests"),
        Cmd::Claim { id } => cli.post(agent, &format!("/api/requests/{id}/claim"), ""),
        Cmd::Respond { id, file, payload } => {
            let payload = read_payload(file.as_deref(), payload.as_deref());
            let body = serde_json::json!({ "response": payload }).to_string();
            cli.post(agent, &format!("/api/requests/{id}/respond"), &body)
        }
    }
}

fn read_payload(file: Option<&Path>, payload: Option<&str>) -> Value {
    match (file, payload) {
        (Some(p), _) => match fs::read_to_string(p) {
            Ok(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
            Err(e) => {
                eprintln!("read {}: {e}", p.display());
                Value::Null
            }
        },
        (None, Some(s)) => serde_json::from_str(s).unwrap_or(Value::String(s.to_string())),
        (None, None) => Value::Null,
    }
}

use std::path::Path;

fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

impl Cli {
    fn auth(&self) -> String {
        format!("Bearer {}", self.token)
    }
    fn get(&self, agent: &ureq::Agent, path: &str) -> Result<String, String> {
        let url = format!("{}{path}", self.url.trim_end_matches('/'));
        agent
            .get(&url)
            .set("Authorization", &self.auth())
            .call()
            .map_err(|e| format!("{e}"))
            .and_then(|r| r.into_string().map_err(|e| format!("read body: {e}")))
    }
    fn post(&self, agent: &ureq::Agent, path: &str, body: &str) -> Result<String, String> {
        let url = format!("{}{path}", self.url.trim_end_matches('/'));
        let mut rb = agent.post(&url).set("Authorization", &self.auth());
        if !body.is_empty() {
            rb = rb.set("Content-Type", "application/json");
        }
        rb.send_string(body)
            .map_err(|e| format!("{e}"))
            .and_then(|r| r.into_string().map_err(|e| format!("read body: {e}")))
    }
    fn delete(&self, agent: &ureq::Agent, path: &str) -> Result<String, String> {
        let url = format!("{}{path}", self.url.trim_end_matches('/'));
        agent
            .delete(&url)
            .set("Authorization", &self.auth())
            .call()
            .map_err(|e| format!("{e}"))
            .and_then(|r| r.into_string().map_err(|e| format!("read body: {e}")))
    }
}
