//! The call plan: every AWS operation the client may call, how to reach it on the wire, and how it pages.
//!
//! It is compiled in (`call-plan.json`), so it is also the allow-list: an operation that is not in it is refused
//! before a request is built. The plan is published with the source, so a customer can read exactly what the binary
//! may call, and generate their IAM policy from it (`qloudpulse-collect policy`).

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::OnceLock;

const EMBEDDED: &str = include_str!("../call-plan.json");

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CallPlan {
    pub version: String,
    /// True while the plan is hand-made rather than generated from qloudpulse's collectors.
    #[serde(default)]
    pub seed: bool,
    pub services: BTreeMap<String, Service>,
    pub operations: Vec<Operation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Protocol {
    /// EC2's own query dialect: lists are `Name.N`.
    Ec2,
    /// `awsQuery`: lists are `Name.member.N`, answers are XML.
    Query,
    #[serde(rename = "awsJson1_0")]
    AwsJson10,
    #[serde(rename = "awsJson1_1")]
    AwsJson11,
    RestJson,
    RestXml,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Service {
    pub protocol: Protocol,
    /// The host's first label: `ec2` in `ec2.eu-west-1.amazonaws.com`.
    pub endpoint_prefix: String,
    /// The SigV4 service name, when it differs from the endpoint prefix.
    #[serde(default)]
    pub signing_name: Option<String>,
    /// `Version=` for the query protocols.
    #[serde(default)]
    pub api_version: Option<String>,
    /// `X-Amz-Target` prefix for the JSON protocols.
    #[serde(default)]
    pub target_prefix: Option<String>,
    /// A global service answered in one region only.
    #[serde(default)]
    pub global_region: Option<String>,
    /// A service with one global host (CloudFront: `cloudfront.amazonaws.com`), signed in `global_region`.
    #[serde(default)]
    pub global_host: Option<String>,
    /// JSON with `awsQueryCompatible` (CloudWatch): the SDK sends `x-amzn-query-mode: true`.
    #[serde(default)]
    pub query_compatible: bool,
}

impl Service {
    pub fn signing_name(&self) -> &str {
        self.signing_name.as_deref().unwrap_or(&self.endpoint_prefix)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Operation {
    /// The qloudpulse collector that needs it, or `client` for the client's own planning calls (never replayed).
    pub collector: String,
    /// The key in `services`.
    pub service: String,
    pub name: String,
    pub iam_action: String,
    /// REST protocols: method and path template, `{name}` filled from the parameter of that name.
    #[serde(default)]
    pub http: Option<HttpBinding>,
    #[serde(default)]
    pub paging: Option<Paging>,
    /// Only when this config option is on, e.g. `forecast`. None: always.
    #[serde(default)]
    pub option: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HttpBinding {
    pub method: String,
    pub path: String,
    /// A host label filled from a parameter: `{Bucket}.` (S3, virtual-hosted), `{AccountId}.` (S3 Control).
    #[serde(default)]
    pub host_prefix: Option<String>,
    /// Parameters sent as headers: parameter name -> header name.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Paging {
    /// The request parameter that carries the token (wire name).
    pub input_token: String,
    /// The answer's token field, found at the top of the answer or one level down.
    pub output_token: String,
}

impl CallPlan {
    pub fn embedded() -> &'static CallPlan {
        static PLAN: OnceLock<CallPlan> = OnceLock::new();
        PLAN.get_or_init(|| {
            let plan: CallPlan = serde_json::from_str(EMBEDDED).expect("call-plan.json is valid");
            plan.check().expect("call-plan.json is consistent");
            plan
        })
    }

    fn check(&self) -> Result<()> {
        for op in &self.operations {
            let service = self
                .services
                .get(&op.service)
                .with_context(|| format!("{} names unknown service {}", op.name, op.service))?;
            let rest = matches!(service.protocol, Protocol::RestJson | Protocol::RestXml);
            if rest != op.http.is_some() {
                bail!(
                    "{}:{} needs an http binding exactly when its protocol is REST",
                    op.service,
                    op.name
                );
            }
            if !op.iam_action.contains(':') || op.iam_action.split(':').any(str::is_empty) {
                bail!("{}:{} has a malformed IAM action", op.service, op.name);
            }
        }
        Ok(())
    }

    /// The allow-list check: the operation, or an error naming it.
    pub fn operation(&self, service: &str, name: &str) -> Result<(&Service, &Operation)> {
        let op = self
            .operations
            .iter()
            .find(|o| o.service == service && o.name == name)
            .with_context(|| format!("{service}:{name} is not in the call plan, so the client may not call it"))?;
        Ok((&self.services[&op.service], op))
    }

    /// The IAM actions the collectors need, for the read-only policy, given the options that are on.
    pub fn iam_actions(&self, options: &[String]) -> Vec<String> {
        let mut actions: Vec<String> = self
            .operations
            .iter()
            .filter(|o| o.option.as_ref().is_none_or(|opt| options.contains(opt)))
            .map(|o| o.iam_action.clone())
            .collect();
        actions.sort();
        actions.dedup();
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_plan_loads_and_refuses_unknown_operations() {
        let plan = CallPlan::embedded();
        assert!(plan.operation("ec2", "DescribeInstances").is_ok());
        let err = plan.operation("ec2", "TerminateInstances").unwrap_err().to_string();
        assert!(err.contains("not in the call plan"), "{err}");
    }

    #[test]
    fn every_action_is_read_only() {
        let writes = [
            "Create",
            "Delete",
            "Put",
            "Update",
            "Modify",
            "Terminate",
            "Run",
            "Start",
            "Stop",
            "Attach",
        ];
        for op in &CallPlan::embedded().operations {
            let verb = op.iam_action.split(':').nth(1).unwrap();
            // Athena's queries are reads over the bill, which are named Start/Stop.
            if op.service == "athena" {
                continue;
            }
            assert!(
                !writes.iter().any(|w| verb.starts_with(w)),
                "{} is not a read",
                op.iam_action
            );
        }
    }
}
