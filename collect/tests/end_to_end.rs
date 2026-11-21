//! A whole hourly cycle against a fake AWS: collect every collector's calls, pack with redaction, open and verify the
//! bundle, analyze it. No network.

use anyhow::Result;
use aws_credential_types::Credentials;
use qloudpulse_collect::aws::caller::Account;
use qloudpulse_collect::aws::{Caller, HttpRequest, HttpResponse, Transport};
use qloudpulse_collect::callplan::CallPlan;
use qloudpulse_collect::collect::Run;
use qloudpulse_collect::config::Config;
use qloudpulse_collect::pseudonym::Pseudonymizer;
use qloudpulse_collect::snapshot::{Snapshot, SnapshotMeta};
use qloudpulse_collect::window::Window;
use qloudpulse_collect::{analyze, bundle};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Answers by operation; anything it does not know gets an empty answer in the right format.
struct FakeAws {
    seen: Mutex<Vec<String>>,
}

fn operation(request: &HttpRequest) -> String {
    let header = |name: &str| request.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    if let Some(target) = header("x-amz-target") {
        return target.rsplit('.').next().unwrap().to_string();
    }
    let body = String::from_utf8_lossy(&request.body);
    let form: BTreeMap<String, String> = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
    if let Some(action) = form.get("Action") {
        return action.clone();
    }
    let path = request.url.split(".amazonaws.com").nth(1).unwrap_or("");
    format!("{} {}", request.method, path.split('?').next().unwrap_or(path))
}

impl Transport for FakeAws {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
        assert!(
            request.headers.iter().any(|(k, _)| k == "authorization"),
            "every request is signed"
        );
        let op = operation(request);
        self.seen.lock().unwrap().push(op.clone());
        let body: String = match op.as_str() {
            "DescribeInstances" => "<DescribeInstancesResponse><reservationSet><item><instancesSet><item>\
                <instanceId>i-0abc</instanceId><instanceType>m5.large</instanceType><platformDetails>Linux/UNIX</platformDetails>\
                <tagSet><item><key>Name</key><value>web-prod</value></item><item><key>Owner</key><value>alice@example.com</value></item>\
                <item><key>Team</key><value>payments</value></item></tagSet>\
                </item></instancesSet></item></reservationSet></DescribeInstancesResponse>"
                .into(),
            "ListTables" => r#"{"TableNames":["orders"]}"#.into(),
            "DescribeTable" => r#"{"Table":{"TableName":"orders","BillingModeSummary":{"BillingMode":"PAY_PER_REQUEST"}}}"#.into(),
            "DescribeContinuousBackups" => {
                r#"{"ContinuousBackupsDescription":{"PointInTimeRecoveryDescription":{"PointInTimeRecoveryStatus":"ENABLED"}}}"#.into()
            }
            "DescribeKey" => return Ok(HttpResponse { status: 400, headers: vec![], body: br#"{"__type":"NotFoundException"}"#.to_vec() }),
            "ListKeys" => r#"{"Keys":[{"KeyId":"k1"}],"Truncated":false}"#.into(),
            "GET /clusters" => r#"{"clusters":["prod"]}"#.into(),
            "GET /clusters/prod" => r#"{"cluster":{"name":"prod","version":"1.30","tags":{"Team":"platform","secret-ish":"x"}}}"#.into(),
            o if o.starts_with("GET /cluster-versions") => r#"{"clusterVersions":[]}"#.into(),
            o if o.starts_with("GET ") || o.starts_with("POST ") => "{}".into(),
            o if request.headers.iter().any(|(k, v)| k == "content-type" && v.contains("json")) => {
                let _ = o;
                "{}".into()
            }
            o => format!("<{o}Response><{o}Result/></{o}Response>"),
        };
        Ok(HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: body.into_bytes(),
        })
    }
}

#[test]
fn hourly_cycle_collects_redacts_packs_and_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let config_text = format!(
        "source_id = \"main\"\noutput_dir = \"{}\"\nwork_dir = \"{}\"\n[tags]\ndefault = \"deny\"\nkeep = [\"Name\", \"Team\"]\npseudonymize = [\"Owner\"]\n",
        dir.path().join("out").display(),
        dir.path().join("work").display()
    );
    let config: Config = toml::from_str(&config_text).unwrap();
    config.check().unwrap();

    let fake = FakeAws {
        seen: Mutex::new(vec![]),
    };
    let mut caller = Caller::new(CallPlan::embedded(), &fake, vec![]);
    caller.backoff = std::time::Duration::ZERO;
    let now: jiff::Timestamp = "2026-11-21T07:05:00Z".parse().unwrap();
    let window = Window::last_full_hour(now);
    let account = Account {
        id: "111122223333".into(),
        credentials: Credentials::new("AKID", "secret", None, None, "test"),
    };
    let run = Run {
        caller: &caller,
        window,
        spot_history: false,
    };
    let outcome = run.collect(&[account], &["eu-west-1".to_string()]);
    assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);

    let seen = fake.seen.lock().unwrap().clone();
    for expected in [
        "DescribeInstances",
        "DescribeSpotPriceHistory",
        "DescribeTable",
        "DescribeContinuousBackups",
        "DescribeAlarms",
        "GET /clusters/prod",
    ] {
        assert!(
            seen.iter().any(|s| s == expected),
            "{expected} was not called: {seen:?}"
        );
    }
    assert_eq!(
        seen.iter().filter(|s| *s == "ListTables").count(),
        1,
        "the repeated listing is answered from memory"
    );
    assert!(
        !seen.iter().any(|s| s == "ListSecrets" || s == "GetMetricStatistics"),
        "usageMetrics reads stay off"
    );

    let records = caller.into_records();
    let meta = SnapshotMeta {
        tool_version: "test".into(),
        call_plan_version: CallPlan::embedded().version.clone(),
        window,
        collected_at: now,
        outcome,
    };
    let snapshot = Snapshot::write(dir.path().join("work/raw/x"), meta, records).unwrap();
    let raw = analyze::analyze("raw", &snapshot.records, Some(&config), true);
    assert!(
        raw.tags
            .iter()
            .any(|t| t.key == "Owner" && t.samples.contains(&"alice@example.com".to_string()))
    );

    let pseudonyms = Pseudonymizer::new(vec![3; 32], false);
    let path = bundle::pack(&config, config_text.as_bytes(), &pseudonyms, &snapshot).unwrap();
    let opened = bundle::open(&path).unwrap();
    let all: String = opened.records.iter().map(|r| r.response.body.clone()).collect();
    assert!(!all.contains("alice@example.com"), "pseudonymized");
    assert!(all.contains(&pseudonyms.pseudonym("alice@example.com")));
    assert!(!all.contains("secret-ish"), "denied by default");
    assert!(all.contains("payments") && all.contains("platform"), "kept");
    assert!(
        opened
            .records
            .iter()
            .any(|r| r.error_code.as_deref() == Some("NotFoundException")),
        "errors are recorded"
    );
    assert!(opened.records.iter().all(|r| r.collector == "aws-cost-collect"));

    let report = analyze::analyze("bundle", &opened.records, Some(&config), true);
    assert!(!report.tags.iter().any(|t| t.key == "secret-ish"));
    assert_eq!(opened.manifest.source_id, "main");
    assert!(opened.manifest.call_plan_seed);
}
