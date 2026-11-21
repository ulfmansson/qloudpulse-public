//! A last look before a bundle is written: secrets and personal data in what would leave, after redaction.
//!
//! Blocking kinds (keys, tokens, private keys, passwords, email addresses) stop `pack` until each is redacted by a
//! rule or allowed in `[scan]`. Warning kinds (public IP addresses) are reported and counted in the manifest but do
//! not block: EC2's own answers are full of them. Findings carry a masked sample, never the value.

use crate::body::Body;
use crate::record::Record;
use crate::redact::visit_tags;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Block,
    Warn,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KindSummary {
    pub severity: Option<Severity>,
    pub count: usize,
    pub operations: BTreeSet<String>,
    /// Masked, at most three: `al…om`.
    pub samples: Vec<String>,
    /// Allowed by `[scan] allow`: counted, not blocking.
    #[serde(default)]
    pub allowed: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ScanReport {
    pub kinds: BTreeMap<String, KindSummary>,
}

impl ScanReport {
    /// The kinds that stop `pack`.
    pub fn blocking(&self) -> Vec<(&String, &KindSummary)> {
        self.kinds
            .iter()
            .filter(|(_, k)| k.severity == Some(Severity::Block) && !k.allowed)
            .collect()
    }

    pub fn text(&self) -> String {
        let mut out = String::new();
        for (kind, k) in &self.kinds {
            let state = match (k.severity, k.allowed) {
                (_, true) => "allowed",
                (Some(Severity::Block), _) => "blocks pack",
                _ => "warning",
            };
            out.push_str(&format!(
                "  {kind}: {} ({state}) in {}; e.g. {}\n",
                k.count,
                k.operations.iter().cloned().collect::<Vec<_>>().join(", "),
                k.samples.join(", ")
            ));
        }
        out
    }
}

/// `[scan]` in the config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanConfig {
    /// Kinds that are reported but do not block, e.g. `["email"]` when every address is a role mailbox.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Values to skip, as regexes matched against the whole value: `["^noreply@example\\.com$"]`.
    #[serde(default)]
    pub allow_values: Vec<String>,
}

struct Rule {
    kind: &'static str,
    severity: Severity,
    re: Regex,
}

fn rules() -> &'static [Rule] {
    static R: OnceLock<Vec<Rule>> = OnceLock::new();
    R.get_or_init(|| {
        let r = |kind, severity, re: &str| Rule { kind, severity, re: Regex::new(re).expect("valid") };
        vec![
            r("aws-access-key", Severity::Block, r"\b(?:AKIA|ASIA)[A-Z0-9]{16}\b"),
            r("aws-secret-key", Severity::Block, r#"(?i)(?:aws_secret_access_key|secretaccesskey)["':=\s]+[A-Za-z0-9/+=]{40}"#),
            r("private-key", Severity::Block, r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
            r("jwt", Severity::Block, r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}"),
            r("github-token", Severity::Block, r"\bgh[pousr]_[A-Za-z0-9]{36}\b"),
            r("slack-token", Severity::Block, r"\bxox[baprs]-[A-Za-z0-9-]{10,}"),
            r("password-in-url", Severity::Block, r"\b[a-z][a-z0-9+.-]*://[^/\s:@<>]+:[^/\s@<>]+@"),
            r(
                "secret-assignment",
                Severity::Block,
                r#"(?i)\b(?:password|passwd|pwd|secret|api[_-]?key|access[_-]?token|auth[_-]?token)\b\s*[:=]\s*["']?[^\s"'<>,;&]{6,}"#,
            ),
            r("email", Severity::Block, r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}\b"),
            r("public-ip", Severity::Warn, r"\b(?:\d{1,3}\.){3}\d{1,3}\b"),
        ]
    })
}

/// A tag key that names a secret: its value is a finding whatever it looks like.
fn secret_tag_key() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)(password|passwd|secret|api[_-]?key|token|credential)").expect("valid"))
}

fn is_public(ip: &str) -> bool {
    let Ok(a) = ip.parse::<Ipv4Addr>() else { return false };
    let o = a.octets();
    !(a.is_private()
        || a.is_loopback()
        || a.is_link_local()
        || a.is_unspecified()
        || a.is_broadcast()
        || a.is_documentation()
        || (o[0] == 100 && (64..128).contains(&o[1])))
}

fn mask(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 6 {
        return "…".into();
    }
    format!(
        "{}…{}",
        chars[..2].iter().collect::<String>(),
        chars[chars.len() - 2..].iter().collect::<String>()
    )
}

pub fn scan(records: &[Record], config: &ScanConfig) -> ScanReport {
    let allow_values: Vec<Regex> = config.allow_values.iter().filter_map(|p| Regex::new(p).ok()).collect();
    let mut report = ScanReport::default();
    let mut add = |kind: &str, severity: Severity, value: &str, operation: &str| {
        if allow_values.iter().any(|r| r.is_match(value)) {
            return;
        }
        let k = report.kinds.entry(kind.to_string()).or_default();
        k.severity = Some(severity);
        k.allowed = config.allow.iter().any(|a| a == kind);
        k.count += 1;
        k.operations.insert(operation.to_string());
        let m = mask(value);
        if k.samples.len() < 3 && !k.samples.contains(&m) {
            k.samples.push(m);
        }
    };
    for r in records {
        let op = format!("{}:{}", r.service, r.operation);
        let text = &r.response.body;
        for rule in rules() {
            for m in rule.re.find_iter(text) {
                if rule.kind == "public-ip" && !is_public(m.as_str()) {
                    continue;
                }
                add(rule.kind, rule.severity, m.as_str(), &op);
            }
        }
        visit_tags(&Body::parse(text.as_bytes()), &mut |t| {
            if secret_tag_key().is_match(t.key) && !t.value.is_empty() {
                add("secret-tag", Severity::Block, t.value, &op);
            }
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::protocol::WireRequest;
    use crate::record::RecordedResponse;

    fn record(body: &str) -> Record {
        Record {
            collector: "aws-cost-collect".into(),
            account: "111122223333".into(),
            region: "eu-west-1".into(),
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
        }
    }

    #[test]
    fn finds_secrets_and_people_but_not_private_ips() {
        let body = "<R><tagSet><item><key>Owner</key><value>alice@corp.example</value></item>\
                    <item><key>DbPassword</key><value>hunter2hunter2</value></item></tagSet>\
                    <privateIpAddress>10.1.2.3</privateIpAddress><ipAddress>52.1.2.3</ipAddress>\
                    <userData>AKIAABCDEFGHIJKLMNOP</userData></R>";
        let report = scan(&[record(body)], &ScanConfig::default());
        let kinds: Vec<&str> = report.kinds.keys().map(String::as_str).collect();
        assert_eq!(kinds, ["aws-access-key", "email", "public-ip", "secret-tag"]);
        assert_eq!(report.kinds["email"].samples, ["al…le"], "masked, never the value");
        assert_eq!(report.blocking().len(), 3, "a public IP warns, it does not block");
    }

    #[test]
    fn allowed_kinds_and_values_do_not_block() {
        let body = "<R><a>ops@corp.example</a><b>noreply@corp.example</b></R>";
        let config = ScanConfig {
            allow: vec![],
            allow_values: vec!["^noreply@".into()],
        };
        assert_eq!(scan(&[record(body)], &config).kinds["email"].count, 1);
        let config = ScanConfig {
            allow: vec!["email".into()],
            allow_values: vec![],
        };
        assert!(scan(&[record(body)], &config).blocking().is_empty());
    }

    #[test]
    fn pseudonyms_and_ids_are_not_findings() {
        let body = "<R><value>p_3f9a0c1d2e3f4a5b</value><arn>arn:aws:iam::111122223333:role/x</arn><id>i-0123456789abcdef0</id></R>";
        assert!(scan(&[record(body)], &ScanConfig::default()).kinds.is_empty());
    }
}
