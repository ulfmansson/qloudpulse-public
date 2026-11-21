//! What a snapshot or a bundle holds: every tag key with how often it appears, how many values it has, and what the
//! config does with it; calls by operation; errors; and what the rules cost in qloudpulse's reports.

use crate::body::Body;
use crate::config::{Config, FEATURE_TAGS};
use crate::record::Record;
use crate::redact::visit_tags;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

#[derive(Debug, Serialize)]
pub struct Report {
    pub source: String,
    pub records: usize,
    /// `service:Operation` -> records
    pub operations: BTreeMap<String, usize>,
    /// `service:Operation code` -> records
    pub errors: BTreeMap<String, usize>,
    pub tags: Vec<TagReport>,
    pub feature_impact: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct TagReport {
    pub key: String,
    pub count: usize,
    pub distinct_values: usize,
    /// A few values, cut short. Absent with `--no-values`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<String>,
    /// What the config does with it, when a config was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
    pub operations: BTreeSet<String>,
}

/// Distinct values are counted up to this many per key.
const DISTINCT_CAP: usize = 10_000;

pub fn analyze(source: &str, records: &[Record], config: Option<&Config>, show_values: bool) -> Report {
    struct Acc {
        count: usize,
        values: BTreeSet<String>,
        operations: BTreeSet<String>,
    }
    let mut tags: BTreeMap<String, Acc> = BTreeMap::new();
    let mut operations = BTreeMap::new();
    let mut errors = BTreeMap::new();
    for r in records {
        let op = format!("{}:{}", r.service, r.operation);
        *operations.entry(op.clone()).or_insert(0) += 1;
        if let Some(code) = &r.error_code {
            *errors.entry(format!("{op} {code}")).or_insert(0) += 1;
        }
        let body = Body::parse(r.response.body.as_bytes());
        visit_tags(&body, &mut |t| {
            let acc = tags.entry(t.key.to_string()).or_insert_with(|| Acc {
                count: 0,
                values: BTreeSet::new(),
                operations: BTreeSet::new(),
            });
            acc.count += 1;
            if acc.values.len() < DISTINCT_CAP {
                acc.values.insert(t.value.to_string());
            }
            acc.operations.insert(op.clone());
        });
    }
    let tags = tags
        .into_iter()
        .map(|(key, acc)| TagReport {
            samples: if show_values {
                acc.values.iter().take(3).map(|v| shorten(v)).collect()
            } else {
                vec![]
            },
            action: config.map(|c| c.tag_action(&key).label().to_string()),
            feature: FEATURE_TAGS.iter().find(|(k, _)| *k == key).map(|(_, f)| f.to_string()),
            distinct_values: acc.values.len(),
            count: acc.count,
            operations: acc.operations,
            key,
        })
        .collect();
    Report {
        source: source.to_string(),
        records: records.len(),
        operations,
        errors,
        tags,
        feature_impact: config.map(Config::feature_impact).unwrap_or_default(),
    }
}

fn shorten(v: &str) -> String {
    if v.chars().count() > 40 {
        format!("{}…", v.chars().take(39).collect::<String>())
    } else {
        v.to_string()
    }
}

impl Report {
    pub fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{}: {} recorded calls", self.source, self.records);
        let _ = writeln!(out, "\nTag keys ({}):", self.tags.len());
        let width = self.tags.iter().map(|t| t.key.len()).max().unwrap_or(3).max(3);
        let _ = writeln!(
            out,
            "  {:width$}  {:>7}  {:>8}  {:12}  samples",
            "key", "tags", "values", "action"
        );
        for t in &self.tags {
            let _ = writeln!(
                out,
                "  {:width$}  {:>7}  {:>8}  {:12}  {}{}",
                t.key,
                t.count,
                t.distinct_values,
                t.action.as_deref().unwrap_or("-"),
                t.samples.join(", "),
                t.feature
                    .as_ref()
                    .map(|f| format!("  [qloudpulse: {f}]"))
                    .unwrap_or_default(),
            );
        }
        if !self.feature_impact.is_empty() {
            let _ = writeln!(out, "\nWhat the rules cost in qloudpulse:");
            for line in &self.feature_impact {
                let _ = writeln!(out, "  - {line}");
            }
        }
        if !self.errors.is_empty() {
            let _ = writeln!(out, "\nErrors recorded (replayed to qloudpulse as the same error):");
            for (k, n) in &self.errors {
                let _ = writeln!(out, "  {k}: {n}");
            }
        }
        let _ = writeln!(out, "\nCalls by operation:");
        for (k, n) in &self.operations {
            let _ = writeln!(out, "  {k}: {n}");
        }
        out
    }

    /// A `[tags]` section listing every key found, for pasting into the config.
    pub fn draft_rules(&self) -> String {
        let mut out = String::from("[tags]\ndefault = \"deny\"\nkeep = [\n");
        for t in &self.tags {
            let note = t
                .feature
                .as_deref()
                .map(|f| format!("  # qloudpulse: {f}"))
                .unwrap_or_default();
            let line = format!("\"{}\",{note}", t.key.replace('"', "\\\""));
            if t.feature.is_some() {
                let _ = writeln!(out, "  {line}");
            } else {
                let _ = writeln!(out, "  # {line}");
            }
        }
        out.push_str("]\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::protocol::WireRequest;
    use crate::record::RecordedResponse;

    #[test]
    fn counts_tag_keys_values_and_feature_impact() {
        let body = "<R><tagSet><item><key>Team</key><value>a</value></item><item><key>Owner</key><value>x@y</value></item>\
                    </tagSet><tagSet><item><key>Team</key><value>b</value></item></tagSet></R>";
        let record = Record {
            collector: "aws-cost-collect".into(),
            account: "1".into(),
            region: "r".into(),
            service: "ec2".into(),
            operation: "DescribeInstances".into(),
            page: 1,
            request: WireRequest {
                method: "POST".into(),
                host: "h".into(),
                path: "/".into(),
                query: vec![],
                headers: vec![],
                body: String::new(),
            },
            response: RecordedResponse {
                status: 200,
                headers: vec![],
                body: body.into(),
            },
            error_code: None,
        };
        let config: Config = toml::from_str("source_id = \"a\"\n[tags]\ndrop = [\"Team\"]\n").unwrap();
        let report = analyze("raw snapshot", &[record], Some(&config), true);
        let team = report.tags.iter().find(|t| t.key == "Team").unwrap();
        assert_eq!((team.count, team.distinct_values), (2, 2));
        assert_eq!(team.action.as_deref(), Some("drop"));
        assert!(report.feature_impact.iter().any(|l| l.contains("Team")));
        assert!(report.draft_rules().contains("\"Team\","));
        assert!(report.draft_rules().contains("# \"Owner\","));
    }
}
