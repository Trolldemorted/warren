#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNew {
    pub name: String,
    pub class: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    pub model: String,
    #[serde(default)]
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentPatch {
    pub name: Option<String>,
    pub class: Option<String>,
    pub model: Option<String>,
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Approved,
    Rejected,
    Responded,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Approved => "approved",
            Status::Rejected => "rejected",
            Status::Responded => "responded",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequestNew {
    pub target_class: String,
    #[serde(default)]
    pub target_type: Option<String>,
    pub payload: Value,
    #[serde(default)]
    pub approved: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequestRespond {
    pub response: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoNew {
    pub target_class: String,
    #[serde(default)]
    pub target_type: Option<String>,
    pub payload: Value,
    #[serde(default)]
    pub approved: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginReq {
    pub password: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginRes {
    pub ok: bool,
}
