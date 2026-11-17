//! The request the client sends and the answer it reads: `estimate.schema.json`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The version of `estimate.schema.json` this client writes and reads.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Pulumi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Create,
    Update,
    Replace,
    Delete,
    /// Left as it is, and sent because a changed resource refers to it. qloudpulse does not answer it on its own.
    Same,
}

impl ChangeKind {
    /// What the comment says about the resource.
    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::Create => "new",
            ChangeKind::Update => "changed",
            ChangeKind::Replace => "replaced",
            ChangeKind::Delete => "removed",
            ChangeKind::Same => "unchanged",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub format_version: u32,
    pub tool: Tool,
    pub resources: Vec<ResourceChange>,
    pub not_priced: Vec<NotPriced>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceChange {
    pub key: String,
    pub change: ChangeKind,
    #[serde(rename = "type")]
    pub type_token: String,
    pub region: Option<String>,
    pub before: Option<Map<String, Value>>,
    pub after: Option<Map<String, Value>>,
    pub unknown: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    /// For each of the catalogue's references of this type, the key of the resource it names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotPriced {
    #[serde(rename = "type")]
    pub type_token: String,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub format_version: u32,
    #[serde(default)]
    pub estimate_id: Option<String>,
    pub currency: String,
    pub resources: Vec<ResourceEstimate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Basis {
    CompanyRate,
    ListPrice,
    NotPriced,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceEstimate {
    pub key: String,
    #[serde(default)]
    pub monthly_before: Option<f64>,
    #[serde(default)]
    pub monthly_after: Option<f64>,
    pub basis: Basis,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoteKind {
    Saving,
    Usage,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Note {
    pub kind: NoteKind,
    pub text: String,
    #[serde(default)]
    pub monthly_saving: Option<f64>,
    #[serde(default)]
    pub risk: Option<Risk>,
}
