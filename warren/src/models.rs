#![allow(dead_code)]

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::fmt;

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
    pub payload: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequestRespond {
    pub response: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginReq {
    pub password: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginRes {
    pub ok: bool,
}
