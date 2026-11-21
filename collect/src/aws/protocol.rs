//! Building a request for each AWS wire protocol, and finding the next page's token in an answer.
//!
//! Parameters are given in their **wire** names (EC2's `Filter`, not the SDKs' `Filters`), because the recording is
//! matched on the wire request when qloudpulse replays it.

use crate::body::Body;
use crate::callplan::{Operation, Protocol, Service};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The request as recorded: what was asked, without the signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireRequest {
    pub method: String,
    pub host: String,
    pub path: String,
    /// Sorted; a name repeats for a list (EKS's `clusterVersions`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
}

impl WireRequest {
    pub fn url(&self) -> String {
        let mut url = format!("https://{}{}", self.host, self.path);
        if !self.query.is_empty() {
            let pairs: Vec<String> = self
                .query
                .iter()
                .map(|(k, v)| format!("{}={}", encode_label(k), encode_label(v)))
                .collect();
            url.push('?');
            url.push_str(&pairs.join("&"));
        }
        url
    }
}

pub fn host(service: &Service, region: &str) -> String {
    match &service.global_host {
        Some(h) => h.clone(),
        None => format!("{}.{}.amazonaws.com", service.endpoint_prefix, region),
    }
}

/// The request for one page. `params` is a JSON object of wire parameters.
pub fn build(
    service: &Service,
    op: &Operation,
    region: &str,
    params: &Value,
    token: Option<&str>,
) -> Result<WireRequest> {
    build_with(service, op, region, params, token, false)
}

/// [`build`], with `path_style` for a custom endpoint (`AWS_ENDPOINT_URL`): a bucket goes in the path, and an account
/// host label is left out (S3 Control also sends it as a header), as the SDKs do for an endpoint of your own.
pub fn build_with(
    service: &Service,
    op: &Operation,
    region: &str,
    params: &Value,
    token: Option<&str>,
    path_style: bool,
) -> Result<WireRequest> {
    let params = params.as_object().cloned().unwrap_or_default();
    let host = host(service, region);
    let token_param = op.paging.as_ref().map(|p| p.input_token.as_str());
    match service.protocol {
        Protocol::Ec2 | Protocol::Query => {
            let version = service
                .api_version
                .as_deref()
                .context("a query service needs api_version")?;
            let mut form = BTreeMap::new();
            form.insert("Action".to_string(), op.name.clone());
            form.insert("Version".to_string(), version.to_string());
            let member = service.protocol == Protocol::Query;
            for (k, v) in &params {
                flatten(k, v, member, &mut form);
            }
            if let (Some(t), Some(name)) = (token, token_param) {
                form.insert(name.to_string(), t.to_string());
            }
            let mut body = url::form_urlencoded::Serializer::new(String::new());
            for (k, v) in &form {
                body.append_pair(k, v);
            }
            Ok(WireRequest {
                method: "POST".into(),
                host,
                path: "/".into(),
                query: Vec::new(),
                headers: vec![(
                    "content-type".into(),
                    "application/x-www-form-urlencoded; charset=utf-8".into(),
                )],
                body: body.finish(),
            })
        }
        Protocol::AwsJson10 | Protocol::AwsJson11 => {
            let prefix = service
                .target_prefix
                .as_deref()
                .context("a JSON service needs target_prefix")?;
            let mut body = params;
            if let (Some(t), Some(name)) = (token, token_param) {
                body.insert(name.to_string(), Value::String(t.to_string()));
            }
            let version = if service.protocol == Protocol::AwsJson10 {
                "1.0"
            } else {
                "1.1"
            };
            let mut headers = vec![
                ("content-type".into(), format!("application/x-amz-json-{version}")),
                ("x-amz-target".into(), format!("{prefix}.{}", op.name)),
            ];
            if service.query_compatible {
                headers.push(("x-amzn-query-mode".into(), "true".into()));
            }
            Ok(WireRequest {
                method: "POST".into(),
                host,
                path: "/".into(),
                query: Vec::new(),
                headers,
                body: Value::Object(body).to_string(),
            })
        }
        Protocol::RestJson | Protocol::RestXml => {
            let binding = op.http.as_ref().context("a REST operation needs an http binding")?;
            let mut rest = params;
            let (template, fixed_query) = binding.path.split_once('?').unwrap_or((&binding.path, ""));
            let mut path = fill_path(template, &mut rest)?;
            // Headers read before the host label is filled: S3 Control sends the account in both.
            let mut extra_headers = Vec::new();
            for (param, header) in &binding.headers {
                if let Some(v) = rest.get(param) {
                    extra_headers.push((header.to_ascii_lowercase(), scalar(v).context("header parameter")?));
                }
            }
            let mut host = host;
            if let Some(prefix) = &binding.host_prefix {
                let label = fill_path(prefix, &mut rest)?;
                let label = label.trim_end_matches('.');
                let bucket = prefix.starts_with("{Bucket}");
                if path_style && !bucket {
                    // An account label: the header names it.
                } else if path_style || label.contains("%2E") || label.contains('.') {
                    // A bucket name with dots is not a host label: the SDKs use path style for it.
                    path = format!(
                        "/{}{}",
                        label.replace("%2E", "."),
                        if path == "/" { String::new() } else { path }
                    );
                } else {
                    host = format!("{label}.{host}");
                }
            }
            for param in binding.headers.keys() {
                rest.remove(param);
            }
            let mut query: Vec<(String, String)> = url::form_urlencoded::parse(fixed_query.as_bytes())
                .into_owned()
                .collect();
            let mut body = String::new();
            if binding.method == "GET" {
                for (k, v) in rest {
                    match v {
                        Value::Array(items) => {
                            for item in items {
                                query.push((k.clone(), scalar(&item).context("query list item")?));
                            }
                        }
                        v => query.push((
                            k,
                            scalar(&v).with_context(|| format!("query parameter {}", binding.path))?,
                        )),
                    }
                }
                if let (Some(t), Some(name)) = (token, token_param) {
                    query.push((name.to_string(), t.to_string()));
                }
                query.sort();
            } else {
                if let (Some(t), Some(name)) = (token, token_param) {
                    rest.insert(name.to_string(), Value::String(t.to_string()));
                }
                if !rest.is_empty() {
                    body = Value::Object(rest).to_string();
                }
            }
            let mut headers = extra_headers;
            if !body.is_empty() {
                headers.push(("content-type".into(), "application/json".into()));
            }
            Ok(WireRequest {
                method: binding.method.clone(),
                host,
                path,
                query,
                headers,
                body,
            })
        }
    }
}

fn scalar(v: &Value) -> Result<String> {
    Ok(match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => bail!("must be a string, number or boolean"),
    })
}

fn fill_path(template: &str, params: &mut Map<String, Value>) -> Result<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let close = rest[open..].find('}').context("unclosed { in path")? + open;
        let name = &rest[open + 1..close];
        let value = params
            .remove(name)
            .with_context(|| format!("path parameter {name} is missing"))?;
        let value = scalar(&value)?;
        out.push_str(&encode_label(&value));
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// A path label as the AWS SDKs encode it: everything but `A-Z a-z 0-9 - _ . ~` as `%XX`, so an ARN's `:` and `/`
/// are encoded too.
pub fn encode_label(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Query-protocol serialization: `Key.N` (EC2) or `Key.member.N` (awsQuery) for lists, `Key.Sub` for structures.
fn flatten(prefix: &str, value: &Value, member: bool, out: &mut BTreeMap<String, String>) {
    match value {
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let key = if member {
                    format!("{prefix}.member.{}", i + 1)
                } else {
                    format!("{prefix}.{}", i + 1)
                };
                flatten(&key, item, member, out);
            }
        }
        Value::Object(fields) => {
            for (k, v) in fields {
                flatten(&format!("{prefix}.{k}"), v, member, out);
            }
        }
        Value::Null => {}
        other => {
            out.insert(prefix.to_string(), scalar(other).unwrap_or_default());
        }
    }
}

/// The next page's token, if the answer has one.
pub fn next_token(op: &Operation, body: &Body) -> Option<String> {
    let name = &op.paging.as_ref()?.output_token;
    body.find_shallow(name).filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callplan::CallPlan;
    use serde_json::json;

    #[test]
    fn ec2_lists_are_numbered_without_member() {
        let plan = CallPlan::embedded();
        let (service, op) = plan.operation("ec2", "DescribeInstances").unwrap();
        let params =
            json!({"MaxResults": 1000, "Filter": [{"Name": "instance-state-name", "Value": ["running", "stopped"]}]});
        let request = build(service, op, "eu-west-1", &params, Some("tok")).unwrap();
        assert_eq!(request.host, "ec2.eu-west-1.amazonaws.com");
        let form: BTreeMap<String, String> = url::form_urlencoded::parse(request.body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(form["Action"], "DescribeInstances");
        assert_eq!(form["Filter.1.Name"], "instance-state-name");
        assert_eq!(form["Filter.1.Value.2"], "stopped");
        assert_eq!(form["NextToken"], "tok");
    }

    #[test]
    fn json_protocol_sets_target_and_token() {
        let plan = CallPlan::embedded();
        let (service, op) = plan.operation("dynamodb", "ListTables").unwrap();
        let request = build(service, op, "eu-west-1", &json!({}), Some("t1")).unwrap();
        assert!(
            request
                .headers
                .contains(&("x-amz-target".into(), "DynamoDB_20120810.ListTables".into()))
        );
        let body: Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body[&op.paging.as_ref().unwrap().input_token], "t1");
    }

    #[test]
    fn rest_fills_path_and_query() {
        let plan = CallPlan::embedded();
        let (service, op) = plan.operation("eks", "DescribeCluster").unwrap();
        let request = build(service, op, "eu-west-1", &json!({"name": "prod one"}), None).unwrap();
        assert_eq!(request.path, "/clusters/prod%20one");
        assert_eq!(request.method, "GET");
    }

    #[test]
    fn rest_lists_repeat_in_the_query_and_arns_are_encoded() {
        let plan = CallPlan::embedded();
        let (service, op) = plan.operation("eks", "DescribeClusterVersions").unwrap();
        let request = build(
            service,
            op,
            "eu-west-1",
            &json!({"clusterVersions": ["1.30", "1.29"]}),
            None,
        )
        .unwrap();
        assert_eq!(
            request.query,
            vec![
                ("clusterVersions".into(), "1.29".into()),
                ("clusterVersions".into(), "1.30".into())
            ]
        );
        let (service, op) = plan.operation("kafka", "DescribeClusterV2").unwrap();
        let arn = "arn:aws:kafka:eu-west-1:111122223333:cluster/c/uuid";
        let request = build(service, op, "eu-west-1", &json!({"ClusterArn": arn}), None).unwrap();
        assert_eq!(
            request.path,
            "/api/v2/clusters/arn%3Aaws%3Akafka%3Aeu-west-1%3A111122223333%3Acluster%2Fc%2Fuuid"
        );
    }

    #[test]
    fn s3_buckets_go_in_the_host_and_s3_control_takes_the_account() {
        let plan = CallPlan::embedded();
        let (service, op) = plan.operation("s3", "GetBucketAcl").unwrap();
        let r = build(service, op, "eu-north-1", &json!({"Bucket": "acme-logs"}), None).unwrap();
        assert_eq!(r.url(), "https://acme-logs.s3.eu-north-1.amazonaws.com/?acl=");
        let r = build(service, op, "eu-north-1", &json!({"Bucket": "acme.logs"}), None).unwrap();
        assert_eq!(
            r.url(),
            "https://s3.eu-north-1.amazonaws.com/acme.logs?acl=",
            "dotted names go path-style"
        );
        let (service, op) = plan.operation("s3control", "GetPublicAccessBlock").unwrap();
        let r = build(service, op, "eu-west-1", &json!({"AccountId": "111122223333"}), None).unwrap();
        assert_eq!(r.host, "111122223333.s3-control.eu-west-1.amazonaws.com");
        assert!(r.headers.contains(&("x-amz-account-id".into(), "111122223333".into())));
        let (service, op) = plan.operation("cloudfront", "ListDistributions").unwrap();
        let r = build(service, op, "us-east-1", &json!({"MaxItems": 100}), None).unwrap();
        assert_eq!(
            r.url(),
            "https://cloudfront.amazonaws.com/2020-05-31/distribution?MaxItems=100"
        );
    }

    #[test]
    fn s3_list_buckets_keeps_its_fixed_query() {
        let plan = CallPlan::embedded();
        let (service, op) = plan.operation("s3", "ListBuckets").unwrap();
        let request = build(service, op, "us-east-1", &json!({"max-buckets": 1000}), None).unwrap();
        assert_eq!(
            request.url(),
            "https://s3.us-east-1.amazonaws.com/?max-buckets=1000&x-id=ListBuckets"
        );
    }
}
