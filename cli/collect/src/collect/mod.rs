//! What to call: the same requests qloudpulse's collectors make, so the recording answers them when replayed.
//!
//! Each collector here mirrors one in qloudpulse's collect-swamp. Its requests (parameters in wire names, page sizes,
//! which calls follow from which answers) must match that collector's, or the replay answers "access denied" for the
//! request that differs. collect-swamp's contract tests replay this client's fixtures to catch that.

pub mod carbon;
pub mod cost;
pub mod cur;
pub mod forecast;
pub mod infra;

use crate::aws::Caller;
use crate::aws::caller::{Account, AwsError, Unanswered};
use crate::body::Body;
use crate::log;
use crate::window::Window;
use anyhow::Result;
use serde_json::Value;

/// What the run reached, for the manifest and the log. Names of operations and counts only.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Outcome {
    pub accounts: Vec<String>,
    pub regions: Vec<String>,
    /// `account region collector: error`, for each collector that stopped early. The error is in full, so this
    /// stays on this machine: the snapshot and the log. What leaves is [`Outcome::stopped`].
    pub failures: Vec<String>,
    /// The same collectors, in fixed words: what the manifest carries.
    #[serde(default)]
    pub stopped: Vec<Stopped>,
    /// What the bill queries read, when this run read the bill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cur: Option<cur::CurOutcome>,
    /// What the forecast read, when this run read it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forecast: Option<forecast::ForecastOutcome>,
}

/// One collector that stopped early, in fixed words only. Nothing AWS, the network or this machine said is copied
/// into it (an error's text can name a role, a bucket, a table or a path), so it can leave in the manifest and be
/// logged by whoever receives the bundle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stopped {
    pub account: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub region: String,
    pub collector: String,
    /// `aws_error`, `dns`, `timeout`, `connect`, `tls`, `network`, `credentials`, `query_failed` or `other`.
    pub kind: String,
    /// The HTTP status of an `aws_error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The AWS error code of an `aws_error`, e.g. `AccessDenied`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// The service endpoint that gave no answer, from the call plan: `elasticache.ap-southeast-1.amazonaws.com`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

/// Collector name for an account whose role could not be assumed.
pub const ASSUME_ROLE: &str = "assume-role";

impl Stopped {
    pub fn of(account: &str, region: &str, collector: &str, error: &anyhow::Error) -> Stopped {
        let mut stopped = Stopped {
            account: account.to_string(),
            region: region.to_string(),
            collector: collector.to_string(),
            kind: "other".into(),
            status: None,
            code: None,
            host: None,
        };
        if let Some(aws) = error.chain().find_map(|c| c.downcast_ref::<AwsError>()) {
            stopped.kind = "aws_error".into();
            stopped.status = Some(aws.status);
            // A code is a name like `AccessDenied`; anything else in its place is not copied.
            let name = !aws.code.is_empty()
                && aws.code.len() <= 64
                && aws
                    .code
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_');
            stopped.code = name.then(|| aws.code.clone());
        } else if let Some(u) = error.chain().find_map(|c| c.downcast_ref::<Unanswered>()) {
            stopped.kind = u.kind.into();
            stopped.host = Some(u.host.clone());
        } else if error.chain().any(|c| c.is::<cur::QueryFailed>()) {
            stopped.kind = "query_failed".into();
        } else if collector == ASSUME_ROLE {
            stopped.kind = "credentials".into();
        }
        stopped
    }

    /// `account region collector: kind status code host`, the parts it has.
    pub fn line(&self) -> String {
        let mut line = self.account.clone();
        for part in [&self.region, &self.collector] {
            if !part.is_empty() {
                line.push(' ');
                line.push_str(part);
            }
        }
        line.push_str(": ");
        line.push_str(&self.kind);
        if let Some(status) = self.status {
            line.push_str(&format!(" {status}"));
        }
        for part in [&self.code, &self.host].into_iter().flatten() {
            line.push(' ');
            line.push_str(part);
        }
        line
    }
}

impl Outcome {
    /// Notes that `collector` stopped: the error in full in the log and in [`Outcome::failures`], which stay here,
    /// and in fixed words in [`Outcome::stopped`], which leaves. `region` is empty for what has none.
    pub fn fail(&mut self, account: &str, region: &str, collector: &str, error: &anyhow::Error) {
        log::warn(
            "collector stopped",
            &[
                ("account", account.to_string()),
                ("region", region.to_string()),
                ("collector", collector.to_string()),
                ("error", format!("{error:#}")),
            ],
        );
        let place = if region.is_empty() {
            account.to_string()
        } else {
            format!("{account} {region}")
        };
        self.failures.push(format!("{place} {collector}: {error:#}"));
        self.stopped.push(Stopped::of(account, region, collector, error));
    }
}

pub struct Run<'a> {
    pub caller: &'a Caller<'a>,
    pub window: Window,
    /// Also read the 90-day spot price history (once a day).
    pub spot_history: bool,
}

impl Run<'_> {
    pub fn collect(&self, accounts: &[Account], regions: &[String]) -> Outcome {
        let mut outcome = Outcome {
            accounts: accounts.iter().map(|a| a.id.clone()).collect(),
            regions: regions.to_vec(),
            ..Outcome::default()
        };
        for account in accounts {
            for region in regions {
                for (name, collector) in cost::COLLECTORS {
                    if let Err(e) = collector(self, account, region) {
                        outcome.fail(&account.id, region, name, &e);
                    }
                }
            }
        }
        // ECS after all inventory, as qloudpulse's collector does.
        for account in accounts {
            for region in regions {
                if let Err(e) = cost::ecs(self, account, region) {
                    outcome.fail(&account.id, region, "ecs", &e);
                }
            }
        }
        outcome
    }

    pub fn call(
        &self,
        account: &Account,
        region: &str,
        service: &str,
        operation: &str,
        params: Value,
    ) -> Result<Vec<Body>> {
        self.caller
            .call("aws-cost-collect", account, region, service, operation, params)
    }

    /// Like [`Run::call`], but an AWS error with one of `codes` (a resource gone between list and describe) is
    /// recorded and passed over, as the collector does.
    pub fn call_ignoring(
        &self,
        account: &Account,
        region: &str,
        service: &str,
        operation: &str,
        params: Value,
        codes: &[&str],
    ) -> Result<Vec<Body>> {
        match self.call(account, region, service, operation, params) {
            Err(e)
                if e.downcast_ref::<AwsError>()
                    .is_some_and(|a| codes.contains(&a.code.as_str())) =>
            {
                Ok(vec![])
            }
            other => other,
        }
    }
}

/// The client's own planning: the account its credentials belong to. Never replayed, so recorded as `client`.
pub fn caller_account(
    caller: &Caller,
    credentials: &aws_credential_types::Credentials,
    region: &str,
) -> Result<String> {
    let probe = Account {
        id: "self".into(),
        credentials: credentials.clone(),
    };
    let pages = caller.call(
        "client",
        &probe,
        region,
        "sts",
        "GetCallerIdentity",
        Value::Object(Default::default()),
    )?;
    pages
        .first()
        .and_then(Body::xml)
        .and_then(|r| r.child("GetCallerIdentityResult"))
        .and_then(|r| r.text_of("Account"))
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("GetCallerIdentity answered without an account"))
}

/// The regions enabled for the account.
pub fn enabled_regions(caller: &Caller, account: &Account, region: &str) -> Result<Vec<String>> {
    let pages = caller.call(
        "client",
        account,
        region,
        "ec2",
        "DescribeRegions",
        Value::Object(Default::default()),
    )?;
    let mut regions: Vec<String> = pages
        .iter()
        .filter_map(Body::xml)
        .flat_map(|r| r.find_all(&["regionInfo", "item"]))
        .filter_map(|i| i.text_of("regionName").map(str::to_string))
        .collect();
    regions.sort();
    Ok(regions)
}

/// String values found by following `path` through a JSON answer, stepping into arrays on the way.
pub fn json_strings(pages: &[Body], path: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for page in pages.iter().filter_map(Body::json) {
        collect_json(page, path, &mut out);
    }
    out
}

fn collect_json(value: &Value, path: &[&str], out: &mut Vec<String>) {
    match (value, path.split_first()) {
        (Value::Array(items), _) => items.iter().for_each(|i| collect_json(i, path, out)),
        (Value::String(s), None) => out.push(s.clone()),
        (Value::Object(map), Some((first, rest))) => {
            if let Some(v) = map.get(*first) {
                collect_json(v, rest, out);
            }
        }
        _ => {}
    }
}

/// JSON objects found by following `path`, stepping into arrays.
pub fn json_objects<'a>(pages: &'a [Body], path: &[&str]) -> Vec<&'a Value> {
    let mut out = Vec::new();
    for page in pages.iter().filter_map(Body::json) {
        objects(page, path, &mut out);
    }
    out
}

fn objects<'a>(value: &'a Value, path: &[&str], out: &mut Vec<&'a Value>) {
    match (value, path.split_first()) {
        (Value::Array(items), _) => items.iter().for_each(|i| objects(i, path, out)),
        (v @ Value::Object(_), None) => out.push(v),
        (Value::Object(map), Some((first, rest))) => {
            if let Some(v) = map.get(*first) {
                objects(v, rest, out);
            }
        }
        _ => {}
    }
}

/// First-seen order, no repeats.
pub fn distinct(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    items.into_iter().filter(|i| seen.insert(i.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_leaves_of_a_failure_is_fixed_words_only() {
        let role = "arn:aws:iam::111122223333:role/payroll-readers";
        let denied = anyhow::Error::new(AwsError {
            status: 403,
            code: "AccessDenied".into(),
        })
        .context(format!("User {role}/alice@example.com may not read table salaries"));
        let assume = anyhow::anyhow!("sts said no to {role}").context(format!("could not assume {role}"));
        let query = anyhow::Error::new(cur::QueryFailed("FAILED: s3://acme-billing/cur/ not found".into()));
        let odd_code = anyhow::Error::new(AwsError {
            status: 400,
            code: "bucket acme-billing is gone".into(),
        });

        let mut outcome = Outcome::default();
        outcome.fail("111122223333", "eu-west-1", "rds", &denied);
        outcome.fail("111122223333", "", ASSUME_ROLE, &assume);
        outcome.fail("111122223333", "eu-west-1", "cur", &query);
        outcome.fail("111122223333", "eu-west-1", "kms", &odd_code);

        let lines: Vec<String> = outcome.stopped.iter().map(Stopped::line).collect();
        assert_eq!(
            lines,
            [
                "111122223333 eu-west-1 rds: aws_error 403 AccessDenied",
                "111122223333 assume-role: credentials",
                "111122223333 eu-west-1 cur: query_failed",
                "111122223333 eu-west-1 kms: aws_error 400",
            ]
        );
        let leaving = serde_json::to_string(&outcome.stopped).unwrap();
        for private in ["payroll", "alice", "salaries", "acme-billing", "arn:"] {
            assert!(!leaving.contains(private), "{private} in {leaving}");
        }
        // The full text is still there for the person running it.
        assert!(outcome.failures[1].contains("could not assume arn:aws:iam::111122223333:role/payroll-readers"));
    }
}
