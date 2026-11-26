//! Calls an operation through every page, records each page, and answers a repeated identical call from memory.

use super::protocol::{self, WireRequest};
use super::sign::sign_request;
use super::transport::{HttpRequest, HttpResponse, Transport};
use crate::body::Body;
use crate::callplan::CallPlan;
use crate::record::{Record, RecordedResponse, kept_headers};
use anyhow::{Result, bail};
use aws_credential_types::Credentials;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// An AWS error answer. Recorded like any other answer; returned so a collector can treat e.g. a table deleted
/// between list and describe the way qloudpulse's own collector does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsError {
    pub status: u16,
    pub code: String,
}

impl std::fmt::Display for AwsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AWS answered {} {}", self.status, self.code)
    }
}

impl std::error::Error for AwsError {}

#[derive(Clone)]
pub struct Account {
    pub id: String,
    pub credentials: Credentials,
}

/// Throttling and server errors are retried this many times before the last answer is recorded.
const ATTEMPTS: u32 = 6;

const THROTTLING: &[&str] = &[
    "Throttling",
    "ThrottlingException",
    "ThrottledException",
    "RequestThrottledException",
    "TooManyRequestsException",
    "ProvisionedThroughputExceededException",
    "RequestLimitExceeded",
    "RequestThrottled",
    "SlowDown",
    "BandwidthLimitExceeded",
];

pub struct Caller<'a> {
    plan: &'a CallPlan,
    transport: &'a dyn Transport,
    options: Vec<String>,
    records: Mutex<Vec<Record>>,
    memo: Mutex<HashMap<String, std::result::Result<Vec<Body>, AwsError>>>,
    /// Waits between retries; tests set it to zero.
    pub backoff: Duration,
    /// The signing clock; tests fix it.
    pub now: fn() -> SystemTime,
    /// `AWS_ENDPOINT_URL`: send every request there instead of AWS (aws-sim, a proxy), path-style. What is recorded
    /// still names the AWS host the request is for.
    pub endpoint: Option<String>,
}

impl<'a> Caller<'a> {
    pub fn new(plan: &'a CallPlan, transport: &'a dyn Transport, options: Vec<String>) -> Caller<'a> {
        Caller {
            plan,
            transport,
            options,
            records: Mutex::new(Vec::new()),
            memo: Mutex::new(HashMap::new()),
            backoff: Duration::from_millis(500),
            now: SystemTime::now,
            endpoint: None,
        }
    }

    /// How many answers are recorded so far.
    pub fn recorded(&self) -> usize {
        self.records.lock().map(|r| r.len()).unwrap_or(0)
    }

    pub fn into_records(self) -> Vec<Record> {
        self.records
            .into_inner()
            .expect("no thread panicked holding the records")
    }

    /// Every page of `service:operation` with `params` (wire names), in `region`, as `account`. An AWS error on
    /// any page is recorded and returned as [`AwsError`]; the pages before it are recorded too.
    pub fn call(
        &self,
        collector: &str,
        account: &Account,
        region: &str,
        service: &str,
        operation: &str,
        params: Value,
    ) -> Result<Vec<Body>> {
        self.call_inner(collector, account, region, service, operation, params, true)
    }

    /// Like [`Caller::call`], for a request repeated until its answer changes (Athena's GetQueryExecution): never
    /// answered from memory, and only its latest answer is kept, so a replay answers every repeat with it.
    pub fn call_latest(
        &self,
        collector: &str,
        account: &Account,
        region: &str,
        service: &str,
        operation: &str,
        params: Value,
    ) -> Result<Vec<Body>> {
        self.call_inner(collector, account, region, service, operation, params, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn call_inner(
        &self,
        collector: &str,
        account: &Account,
        region: &str,
        service: &str,
        operation: &str,
        params: Value,
        memo: bool,
    ) -> Result<Vec<Body>> {
        let (svc, op) = self.plan.operation(service, operation)?;
        if let Some(option) = &op.option
            && !self.options.contains(option)
        {
            bail!("{service}:{operation} needs the option {option}, which is off");
        }
        let region = svc.global_region.as_deref().unwrap_or(region);
        let path_style = self.endpoint.is_some();
        let first = protocol::build_with(svc, op, region, &params, None, path_style)?;
        let key = serde_json::to_string(&(&account.id, region, service, operation, &first))?;
        if memo && let Some(answer) = self.memo.lock().expect("memo").get(&key) {
            return answer.clone().map_err(Into::into);
        }

        let mut pages = Vec::new();
        let mut request = first;
        let mut page = 1;
        let answer = loop {
            let response = self.send(&request, svc.signing_name(), region, &account.credentials)?;
            let body = Body::parse(&response.body);
            let error = error_of(&response, &body);
            let mut records = self.records.lock().expect("records");
            if !memo {
                records.retain(|r| {
                    !(r.account == account.id
                        && r.region == region
                        && r.service == service
                        && r.operation == operation
                        && r.request == request)
                });
            }
            records.push(Record {
                collector: collector.to_string(),
                account: account.id.clone(),
                region: region.to_string(),
                service: service.to_string(),
                operation: operation.to_string(),
                page,
                request: request.clone(),
                response: RecordedResponse {
                    status: response.status,
                    headers: kept_headers(&response.headers),
                    body: String::from_utf8_lossy(&response.body).into_owned(),
                },
                error_code: error.as_ref().map(|e| e.code.clone()),
            });
            drop(records);
            if let Some(error) = error {
                break Err(error);
            }
            let token = protocol::next_token(op, &body);
            pages.push(body);
            match token {
                Some(token) if page < 10_000 => {
                    request = protocol::build_with(svc, op, region, &params, Some(&token), path_style)?;
                    page += 1;
                }
                _ => break Ok(pages),
            }
        };
        if memo {
            self.memo.lock().expect("memo").insert(key, answer.clone());
        }
        answer.map_err(Into::into)
    }

    fn send(
        &self,
        wire: &WireRequest,
        signing_name: &str,
        region: &str,
        credentials: &Credentials,
    ) -> Result<HttpResponse> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let (url, host) = match &self.endpoint {
                Some(base) => {
                    let base = base.trim_end_matches('/');
                    let aws = wire.url();
                    let rest = aws.split_once(&wire.host).map(|(_, r)| r).unwrap_or("/");
                    let authority = base.split_once("://").map(|(_, a)| a).unwrap_or(base);
                    (
                        format!("{base}{rest}"),
                        authority.split('/').next().unwrap_or(authority).to_string(),
                    )
                }
                None => (wire.url(), wire.host.clone()),
            };
            let mut request = HttpRequest {
                method: wire.method.clone(),
                url,
                headers: wire.headers.clone(),
                body: wire.body.clone().into_bytes(),
            };
            request.headers.push(("host".into(), host));
            sign_request(&mut request, credentials, region, signing_name, (self.now)())?;
            let outcome = self.transport.send(&request);
            let retry = match &outcome {
                Err(_) => true,
                Ok(r) => {
                    r.status == 429
                        || r.status >= 500
                        || error_of(r, &Body::parse(&r.body)).is_some_and(|e| THROTTLING.contains(&e.code.as_str()))
                }
            };
            if !retry || attempt >= ATTEMPTS {
                return outcome;
            }
            let wait = self
                .backoff
                .saturating_mul(1 << (attempt - 1))
                .min(Duration::from_secs(20));
            std::thread::sleep(wait);
        }
    }
}

/// The AWS error code in an answer, from whichever place its protocol puts it.
pub fn error_of(response: &HttpResponse, body: &Body) -> Option<AwsError> {
    if response.status < 300 {
        return None;
    }
    let from_header = response
        .header("x-amzn-query-error")
        .or(response.header("x-amzn-errortype"));
    let code = from_header.map(str::to_string).or_else(|| match body {
        Body::Xml(root) => {
            let error = root
                .child("Errors")
                .and_then(|e| e.child("Error"))
                .or(root.child("Error"));
            match error {
                Some(e) => e.text_of("Code").map(str::to_string),
                None if root.name == "Error" => root.text_of("Code").map(str::to_string),
                None => None,
            }
        }
        Body::Json(v) => v
            .get("__type")
            .or(v.get("code"))
            .or(v.get("Code"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    });
    let code = code.unwrap_or_else(|| format!("HTTP{}", response.status));
    // `aws.protocoltests#Throttling:http://…` or `ThrottlingException;Sender` → `Throttling…`
    let code = code.rsplit('#').next().unwrap_or(&code);
    let code = code.split([':', ';']).next().unwrap_or(code).to_string();
    Some(AwsError {
        status: response.status,
        code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Answers DescribeVolumes in two pages, throttling the first try.
    struct Pages {
        calls: AtomicUsize,
    }

    impl Transport for Pages {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let body = String::from_utf8(request.body.clone()).unwrap();
            let xml = if n == 0 {
                return Ok(HttpResponse {
                    status: 503,
                    headers: vec![],
                    body: b"<Response><Errors><Error><Code>RequestLimitExceeded</Code></Error></Errors></Response>"
                        .to_vec(),
                });
            } else if body.contains("NextToken=") {
                "<DescribeVolumesResponse><volumeSet/></DescribeVolumesResponse>"
            } else {
                "<DescribeVolumesResponse><volumeSet/><nextToken>p2</nextToken></DescribeVolumesResponse>"
            };
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "text/xml".into())],
                body: xml.into(),
            })
        }
    }

    fn account() -> Account {
        Account {
            id: "111122223333".into(),
            credentials: Credentials::new("AKID", "secret", None, None, "test"),
        }
    }

    #[test]
    fn retries_throttling_pages_and_remembers_repeats() {
        let transport = Pages {
            calls: AtomicUsize::new(0),
        };
        let mut caller = Caller::new(CallPlan::embedded(), &transport, vec![]);
        caller.backoff = Duration::ZERO;
        let params = json!({"MaxResults": 500});
        let pages = caller
            .call(
                "aws-cost-collect",
                &account(),
                "eu-west-1",
                "ec2",
                "DescribeVolumes",
                params.clone(),
            )
            .unwrap();
        assert_eq!(pages.len(), 2);
        caller
            .call(
                "aws-cost-collect",
                &account(),
                "eu-west-1",
                "ec2",
                "DescribeVolumes",
                params,
            )
            .unwrap();
        assert_eq!(
            transport.calls.load(Ordering::SeqCst),
            3,
            "a throttled try, two pages, and no repeat"
        );
        let records = caller.into_records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].page, 2);
        assert_eq!(
            records[0].response.headers,
            vec![("content-type".to_string(), "text/xml".to_string())]
        );
    }

    #[test]
    fn refuses_operations_outside_the_plan_and_options() {
        let transport = Pages {
            calls: AtomicUsize::new(0),
        };
        let caller = Caller::new(CallPlan::embedded(), &transport, vec![]);
        let err = caller
            .call("x", &account(), "eu-west-1", "ec2", "TerminateInstances", json!({}))
            .unwrap_err();
        assert!(err.to_string().contains("not in the call plan"));
        let err = caller
            .call("x", &account(), "eu-west-1", "secretsmanager", "ListSecrets", json!({}))
            .unwrap_err();
        assert!(err.to_string().contains("usageMetrics"));
        assert_eq!(transport.calls.load(Ordering::SeqCst), 0);
    }

    /// Records the URL and host of what it is sent.
    struct Seen(std::sync::Mutex<Vec<(String, String)>>);
    impl Transport for Seen {
        fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
            let host = request.headers.iter().find(|(k, _)| k == "host").unwrap().1.clone();
            self.0.lock().unwrap().push((request.url.clone(), host));
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: b"<R/>".to_vec(),
            })
        }
    }

    #[test]
    fn a_custom_endpoint_gets_every_request_path_style() {
        let seen = Seen(std::sync::Mutex::new(vec![]));
        let mut caller = Caller::new(CallPlan::embedded(), &seen, vec![]);
        caller.endpoint = Some("http://127.0.0.1:4599".into());
        caller
            .call(
                "x",
                &account(),
                "eu-north-1",
                "s3",
                "GetBucketAcl",
                json!({"Bucket": "logs"}),
            )
            .unwrap();
        caller
            .call(
                "x",
                &account(),
                "eu-west-1",
                "s3control",
                "GetPublicAccessBlock",
                json!({"AccountId": "111122223333"}),
            )
            .unwrap();
        let seen = seen.0.lock().unwrap().clone();
        assert_eq!(
            seen[0],
            (
                "http://127.0.0.1:4599/logs?acl=".to_string(),
                "127.0.0.1:4599".to_string()
            )
        );
        assert_eq!(
            seen[1].0,
            "http://127.0.0.1:4599/v20180820/configuration/publicAccessBlock"
        );
        let records = caller.into_records();
        assert_eq!(
            records[0].request.host, "s3.eu-north-1.amazonaws.com",
            "the record names the AWS host"
        );
    }

    #[test]
    fn reads_error_codes_of_each_protocol() {
        let json = HttpResponse {
            status: 400,
            headers: vec![],
            body: br#"{"__type":"com.amazonaws.dynamodb.v20120810#ResourceNotFoundException"}"#.to_vec(),
        };
        assert_eq!(
            error_of(&json, &Body::parse(&json.body)).unwrap().code,
            "ResourceNotFoundException"
        );
        let query = HttpResponse {
            status: 403,
            headers: vec![],
            body: b"<ErrorResponse><Error><Code>AccessDenied</Code></Error></ErrorResponse>".to_vec(),
        };
        assert_eq!(
            error_of(&query, &Body::parse(&query.body)).unwrap().code,
            "AccessDenied"
        );
        let rest = HttpResponse {
            status: 404,
            headers: vec![(
                "X-Amzn-ErrorType".into(),
                "ResourceNotFoundException:http://internal".into(),
            )],
            body: b"{}".to_vec(),
        };
        assert_eq!(
            error_of(&rest, &Body::parse(&rest.body)).unwrap().code,
            "ResourceNotFoundException"
        );
    }
}
