//! One recorded exchange with an AWS API: what was asked and the raw answer.

use crate::aws::protocol::WireRequest;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The qloudpulse collector the call serves.
    pub collector: String,
    pub account: String,
    pub region: String,
    pub service: String,
    pub operation: String,
    /// 1 for the first page.
    pub page: u32,
    pub request: WireRequest,
    pub response: RecordedResponse,
    /// The AWS error code, when the answer is an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedResponse {
    pub status: u16,
    /// Only the headers an SDK needs to read the answer: see [`KEPT_HEADERS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Request ids, dates and checksums are left out: they say nothing an SDK needs, and a redacted body would fail a
/// checksum such as DynamoDB's `x-amz-crc32`.
pub const KEPT_HEADERS: &[&str] = &["content-type", "x-amzn-errortype", "x-amzn-query-error"];

pub fn kept_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut kept: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| KEPT_HEADERS.iter().any(|h| k.eq_ignore_ascii_case(h)))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect();
    kept.sort();
    kept
}

/// Records that arrive in the order they were made, with identical requests (same account, region, operation and
/// wire request) recorded once: the collectors sometimes repeat a call, and a replay answers every repeat from the
/// one recording.
pub fn dedupe(records: Vec<Record>) -> Vec<Record> {
    let mut seen = std::collections::HashSet::new();
    records
        .into_iter()
        .filter(|r| {
            let key = serde_json::to_string(&(&r.account, &r.region, &r.service, &r.operation, &r.request))
                .expect("a record serializes");
            seen.insert(key)
        })
        .collect()
}
