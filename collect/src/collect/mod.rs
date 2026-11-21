//! What to call: the same requests qloudpulse's collectors make, so the recording answers them when replayed.
//!
//! Each collector here mirrors one in qloudpulse's collect-swamp. Its requests (parameters in wire names, page sizes,
//! which calls follow from which answers) must match that collector's, or the replay answers "access denied" for the
//! request that differs. collect-swamp's contract tests replay this client's fixtures to catch that.

pub mod cost;

use crate::aws::Caller;
use crate::aws::caller::{Account, AwsError};
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
    /// `account region collector: error`, for each collector that stopped early.
    pub failures: Vec<String>,
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
            failures: Vec::new(),
        };
        for account in accounts {
            for region in regions {
                for (name, collector) in cost::COLLECTORS {
                    if let Err(e) = collector(self, account, region) {
                        log::warn(
                            "collector stopped",
                            &[
                                ("account", account.id.clone()),
                                ("region", region.clone()),
                                ("collector", name.to_string()),
                                ("error", e.to_string()),
                            ],
                        );
                        outcome.failures.push(format!("{} {region} {name}: {e}", account.id));
                    }
                }
            }
        }
        // ECS after all inventory, as qloudpulse's collector does.
        for account in accounts {
            for region in regions {
                if let Err(e) = cost::ecs(self, account, region) {
                    outcome.failures.push(format!("{} {region} ecs: {e}", account.id));
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
