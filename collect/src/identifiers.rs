//! Format-preserving pseudonyms for account IDs and resource IDs, applied everywhere they appear: answers, requests
//! (form fields, JSON bodies, query values, path labels), ARNs inside any of those, and the record's own account.
//!
//! An account ID stays 12 digits and a resource ID keeps its prefix and length (`i-0abc…` → `i-` and as many hex
//! digits), so parsers, ARN checks and joins between the bill and the inventory still work. The same ID always gives
//! the same pseudonym, under the customer's key.

use crate::collect::Outcome;
use crate::config::IdAction;
use crate::pseudonym::Pseudonymizer;
use crate::record::Record;
use regex::{Captures, Regex};
use std::collections::BTreeSet;
use std::sync::OnceLock;

fn twelve_digits() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\b\d{12}\b").expect("valid"))
}

/// Where an account ID is certain to be one: inside an ARN, or in an owner/account field.
fn account_sources() -> &'static [Regex] {
    static R: OnceLock<Vec<Regex>> = OnceLock::new();
    R.get_or_init(|| {
        vec![
            Regex::new(r"arn:aws[a-z-]*:[a-z0-9-]*:[a-z0-9-]*:(\d{12}):").expect("valid"),
            Regex::new(r"<(?:ownerId|OwnerId|requesterId|accountId|AccountId)>(\d{12})<").expect("valid"),
            Regex::new(r#""(?:ownerId|OwnerId|AccountId|accountId|Account)"\s*:\s*"(\d{12})""#).expect("valid"),
            // Athena results: the bill's account_id column holds linked accounts no other answer names.
            Regex::new(r#""VarCharValue"\s*:\s*"(\d{12})""#).expect("valid"),
        ]
    })
}

/// EC2-style resource IDs: a known prefix and 8 or 17 hex digits.
fn resource_id() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"\b(i|vol|eni|nat|vpc|subnet|sg|igw|eigw|tgw-attach|tgw-rtb|tgw|vpce-svc|vpce|cvpn-endpoint|eipalloc|eipassoc|snap|ami|lt|rtb|acl|pcx|vpn|cgw|vgw|dopt|fs|fsap|r)-([0-9a-f]{17}|[0-9a-f]{8})\b",
        )
        .expect("valid")
    })
}

pub struct Identifiers<'a> {
    pub accounts: IdAction,
    pub resources: IdAction,
    pub pseudonyms: &'a Pseudonymizer,
}

/// How many distinct IDs were pseudonymized, for the manifest.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdStats {
    pub account_ids: usize,
    pub resource_ids: usize,
}

impl Identifiers<'_> {
    fn on(&self) -> bool {
        self.accounts == IdAction::Pseudonymize || self.resources == IdAction::Pseudonymize
    }

    pub fn apply(&self, records: &mut [Record], outcome: &mut Outcome) -> IdStats {
        if !self.on() {
            return IdStats::default();
        }
        let mut accounts = BTreeSet::new();
        if self.accounts == IdAction::Pseudonymize {
            accounts.extend(outcome.accounts.iter().cloned());
            for r in records.iter() {
                accounts.insert(r.account.clone());
                for text in [&r.response.body, &r.request.body] {
                    for source in account_sources() {
                        accounts.extend(source.captures_iter(text).map(|c| c[1].to_string()));
                    }
                }
            }
            accounts.retain(|a| a.len() == 12 && a.bytes().all(|b| b.is_ascii_digit()));
        }
        let mut resources = BTreeSet::new();
        let rewrite = |text: &str, resources: &mut BTreeSet<String>| -> String {
            let mut out = text.to_string();
            if !accounts.is_empty() {
                out = twelve_digits()
                    .replace_all(&out, |c: &Captures| {
                        let id = &c[0];
                        if accounts.contains(id) {
                            self.pseudonyms.account_id(id)
                        } else {
                            id.to_string()
                        }
                    })
                    .into_owned();
            }
            if self.resources == IdAction::Pseudonymize {
                out = resource_id()
                    .replace_all(&out, |c: &Captures| {
                        resources.insert(c[0].to_string());
                        self.pseudonyms.resource_id(&c[1], &c[2])
                    })
                    .into_owned();
            }
            out
        };
        for r in records.iter_mut() {
            r.response.body = rewrite(&r.response.body, &mut resources);
            r.account = rewrite(&r.account, &mut resources);
            let form = r
                .request
                .headers
                .iter()
                .any(|(k, v)| k == "content-type" && v.starts_with("application/x-www-form-urlencoded"));
            if form {
                let pairs: Vec<(String, String)> = url::form_urlencoded::parse(r.request.body.as_bytes())
                    .into_owned()
                    .collect();
                let mut out = url::form_urlencoded::Serializer::new(String::new());
                for (k, v) in pairs {
                    out.append_pair(&k, &rewrite(&v, &mut resources));
                }
                r.request.body = out.finish();
            } else {
                r.request.body = rewrite(&r.request.body, &mut resources);
            }
            for (_, v) in r.request.query.iter_mut() {
                *v = rewrite(v, &mut resources);
            }
            r.request.path = r
                .request
                .path
                .split('/')
                .map(|seg| {
                    let decoded = crate::redact::percent_decode(seg);
                    let new = rewrite(&decoded, &mut resources);
                    if new == decoded {
                        seg.to_string()
                    } else {
                        crate::aws::protocol::encode_label(&new)
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
        }
        outcome.accounts = outcome.accounts.iter().map(|a| rewrite(a, &mut resources)).collect();
        outcome.failures = outcome.failures.iter().map(|f| rewrite(f, &mut resources)).collect();
        if let Some(cur) = &mut outcome.cur {
            cur.account = rewrite(&cur.account, &mut resources);
        }
        IdStats {
            account_ids: accounts.len(),
            resource_ids: resources.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::protocol::WireRequest;
    use crate::record::RecordedResponse;

    fn record(account: &str, request_body: &str, path: &str, body: &str) -> Record {
        Record {
            collector: "aws-cost-collect".into(),
            account: account.into(),
            region: "eu-west-1".into(),
            service: "x".into(),
            operation: "Y".into(),
            page: 1,
            request: WireRequest {
                method: "POST".into(),
                host: "h".into(),
                path: path.into(),
                query: vec![],
                headers: vec![(
                    "content-type".into(),
                    "application/x-www-form-urlencoded; charset=utf-8".into(),
                )],
                body: request_body.into(),
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
    fn ids_keep_their_shape_and_match_everywhere() {
        let p = Pseudonymizer::new(vec![5; 32], false);
        let ids = Identifiers {
            accounts: IdAction::Pseudonymize,
            resources: IdAction::Pseudonymize,
            pseudonyms: &p,
        };
        let body = "<R><ownerId>111122223333</ownerId><instanceId>i-0123456789abcdef0</instanceId>\
                    <size>107374182400</size><arn>arn:aws:iam::444455556666:role/x</arn></R>";
        let mut records = vec![record(
            "111122223333",
            "Action=DescribeInstances&InstanceId.1=i-0123456789abcdef0",
            "/api/v2/clusters/arn%3Aaws%3Akafka%3Aeu-west-1%3A111122223333%3Acluster%2Fc",
            body,
        )];
        let mut outcome = Outcome {
            cur: None,
            accounts: vec!["111122223333".into()],
            regions: vec![],
            failures: vec!["111122223333 eu-west-1 x: y".into()],
        };
        let stats = ids.apply(&mut records, &mut outcome);

        let a = p.account_id("111122223333");
        let i = p.resource_id("i", "0123456789abcdef0");
        assert_eq!(a.len(), 12);
        assert!(i.starts_with("i-") && i.len() == "i-0123456789abcdef0".len());
        let r = &records[0];
        assert_eq!(r.account, a);
        assert!(
            r.response.body.contains(&a) && r.response.body.contains(&i),
            "{}",
            r.response.body
        );
        assert!(
            r.response.body.contains(&p.account_id("444455556666")),
            "account inside an ARN"
        );
        assert!(
            r.response.body.contains("107374182400"),
            "a 12-digit size that is no account stays"
        );
        assert!(!r.response.body.contains("111122223333"));
        assert!(r.request.body.contains(&i));
        assert!(r.request.path.contains(&a), "{}", r.request.path);
        assert_eq!(outcome.accounts, vec![a.clone()]);
        assert!(outcome.failures[0].starts_with(&a));
        assert_eq!(
            stats,
            IdStats {
                account_ids: 2,
                resource_ids: 1
            }
        );
    }

    #[test]
    fn keep_changes_nothing() {
        let p = Pseudonymizer::new(vec![5; 32], false);
        let ids = Identifiers {
            accounts: IdAction::Keep,
            resources: IdAction::Keep,
            pseudonyms: &p,
        };
        let mut records = vec![record("111122223333", "", "/", "<x>i-0123456789abcdef0</x>")];
        let mut outcome = Outcome::default();
        ids.apply(&mut records, &mut outcome);
        assert_eq!(records[0].response.body, "<x>i-0123456789abcdef0</x>");
    }
}
