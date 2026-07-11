#![allow(dead_code)]

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use uuid::Uuid;

/// Distinguishes "field omitted" (None) from "field present as null" (Some(None))
/// from "field present with a value" (Some(Some(v))). Plain `Option<Option<T>>`
/// collapses `null` and missing into the same `None`, so we deserialize by hand.
fn deserialize_optional_kind<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<Option<String>>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("optional string or null")
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(Some(v.to_string())))
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(Some(v)))
        }
    }
    deserializer.deserialize_any(V)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentNew {
    pub name: String,
    pub class: String,
    #[serde(default)]
    pub kind: Option<String>,
    pub model: String,
    #[serde(default)]
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub class: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_kind")]
    pub kind: Option<Option<String>>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequestNew {
    pub target_class: String,
    #[serde(default)]
    pub target_type: Option<String>,
    pub payload: String,
    #[serde(default)]
    pub channel_id: Option<Uuid>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequestRespond {
    pub response: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginReq {
    pub password: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginRes {
    pub ok: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelNew {
    pub sender_class: String,
    #[serde(default)]
    pub sender_kind: Option<String>,
    pub receiver_class: String,
    #[serde(default)]
    pub receiver_kind: Option<String>,
    pub description: String,
    #[serde(default = "default_true")]
    pub requires_request_approval: bool,
    #[serde(default = "default_true")]
    pub requires_response_approval: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ChannelPatch {
    #[serde(default)]
    pub sender_class: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_kind")]
    pub sender_kind: Option<Option<String>>,
    #[serde(default)]
    pub receiver_class: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_kind")]
    pub receiver_kind: Option<Option<String>>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub requires_request_approval: Option<bool>,
    #[serde(default)]
    pub requires_response_approval: Option<bool>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct ScheduledPromptNew {
    pub agent_id: Uuid,
    pub name: String,
    pub prompt_text: String,
    pub interval_seconds: i64,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub ignore_inbox_state: bool,
    #[serde(default)]
    pub weekly_safety_buffer_pct: i32,
    #[serde(default)]
    pub session_safety_buffer_pct: i32,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ScheduledPromptPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub prompt_text: Option<String>,
    #[serde(default)]
    pub interval_seconds: Option<i64>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub ignore_inbox_state: Option<bool>,
    #[serde(default)]
    pub weekly_safety_buffer_pct: Option<i32>,
    #[serde(default)]
    pub session_safety_buffer_pct: Option<i32>,
}
