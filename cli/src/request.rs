//! The request the client sends and the answer it reads: `estimate.schema.json`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The version of `estimate.schema.json` this client writes and reads.
pub const FORMAT_VERSION: u32 = 1;

/// The most resources one request may carry.
pub const MAX_RESOURCES: usize = 5000;

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
    /// Left as it is: sent so that the comment can give the whole stack's cost, and so that a changed resource that
    /// refers to it is priced with it. qloudpulse answers it with the same figure before and after and no notes; an
    /// endpoint from before 2026-11-19 does not answer it.
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
    /// The project and stack, `project/stack`, from the resources' URNs when they all agree; an endpoint from before
    /// 2026-11-19 ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// `false` with `--no-store`: qloudpulse prices the change and keeps nothing of it (qloudpulse's ADR 0140). Left out
    /// otherwise, which keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    pub resources: Vec<ResourceChange>,
    pub not_priced: Vec<NotPriced>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceChange {
    pub key: String,
    /// The resource's name, the URN's last part, so that qloudpulse's pages name it (ADR 0140). Never with
    /// `--no-store`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The resource's Pulumi URN. Never with `--no-store`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urn: Option<String>,
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
    /// The estimate's page in qloudpulse's reports. Only from an endpoint that kept the names leaves the client it as
    /// it is; to an older one's it adds the names after `#` (`link::page_link`).
    #[serde(default)]
    pub url: Option<String>,
    /// Whether qloudpulse kept the estimate (ADR 0140): `false` for `--no-store` or a company that keeps nothing, then
    /// with no `estimateId` or `url`. An endpoint from before ADR 0140 leaves it out, and keeps it without the names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored: Option<bool>,
    pub resources: Vec<ResourceEstimate>,
}

impl Response {
    /// Whether qloudpulse says it kept nothing of the estimate, and answers as if it had not: `stored: false`, with no
    /// id and no page.
    pub fn kept_nothing(&self) -> bool {
        self.stored == Some(false) && self.estimate_id.is_none() && self.url.is_none()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(extra: Value) -> Response {
        let mut r = serde_json::json!({"formatVersion": 1, "currency": "USD", "resources": []});
        r.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value(r).unwrap()
    }

    #[test]
    fn only_stored_false_with_no_id_or_page_is_kept_nothing() {
        use serde_json::json;
        assert!(answer(json!({"stored": false})).kept_nothing());
        assert!(
            !answer(json!({})).kept_nothing(),
            "an endpoint from before ADR 0140 keeps it"
        );
        assert!(!answer(json!({"stored": true, "estimateId": "est_01"})).kept_nothing());
        assert!(!answer(json!({"stored": false, "estimateId": "est_01"})).kept_nothing());
        assert!(!answer(json!({"stored": false, "url": "https://q.example/ui/?estimate=e"})).kept_nothing());
    }
}
