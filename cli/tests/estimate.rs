//! The estimate command against Pulumi previews written to Pulumi's `display.PreviewDigest` format.

use std::io::Write as _;
use std::process::{Command, Stdio};

use qloudpulse_cli::catalogue::Catalogue;
use qloudpulse_cli::pulumi::{self, Options, key};
use qloudpulse_cli::render;
use qloudpulse_cli::request::{ChangeKind, Response};
use serde_json::{Value, json};

const CREATE: &str = include_str!("fixtures/preview-create.json");
const UPDATE: &str = include_str!("fixtures/preview-update.json");

const CATALOGUE: &str = include_str!("fixtures/catalogue.json");
const CATALOGUE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/catalogue.json");

fn estimate(json: &str, options: &Options) -> pulumi::Estimate {
    estimate_with(json, CATALOGUE, options)
}

fn estimate_with(json: &str, catalogue: &str, options: &Options) -> pulumi::Estimate {
    pulumi::estimate(
        &pulumi::parse(json).unwrap(),
        &Catalogue::parse(catalogue).unwrap(),
        options,
    )
}

fn resource<'a>(e: &'a pulumi::Estimate, urn: &str) -> &'a qloudpulse_cli::request::ResourceChange {
    let k = key(urn);
    e.request
        .resources
        .iter()
        .find(|r| r.key == k)
        .unwrap_or_else(|| panic!("{urn} was not sent"))
}

#[test]
fn a_new_stack_sends_the_priced_resources_only() {
    let e = estimate(CREATE, &Options::default());
    let types: Vec<&str> = e.request.resources.iter().map(|r| r.type_token.as_str()).collect();
    assert_eq!(
        types,
        [
            "aws:rds/instance:Instance",
            "aws:ec2/instance:Instance",
            "aws:lambda/function:Function"
        ],
        "providers, the stack, components, SQS and random are not priced resources"
    );
    assert!(
        e.request
            .resources
            .iter()
            .all(|r| r.change == ChangeKind::Create && r.before.is_none())
    );

    let not_priced = serde_json::to_value(&e.request.not_priced).unwrap();
    assert_eq!(not_priced, json!([{"type": "aws:sqs/queue:Queue", "count": 2}]));
}

#[test]
fn only_the_listed_attributes_leave_ci() {
    let e = estimate(CREATE, &Options::default());
    let orders = resource(&e, "urn:pulumi:dev::shop::aws:rds/instance:Instance::orders");
    assert_eq!(
        Value::Object(orders.after.clone().unwrap()),
        json!({
            "instanceClass": "db.r6i.xlarge",
            "engine": "postgres",
            "engineVersion": "16.4",
            "multiAz": true,
            "allocatedStorage": 100,
            "storageType": "gp3"
        })
    );
    assert!(
        orders.unknown.is_empty(),
        "the secret password is not a priced attribute, so it is not named"
    );

    let sent = serde_json::to_string(&e.request).unwrap();
    for never in [
        "orders",
        "shop",
        "dev::",
        "urn:",
        "payments",
        "someone@example.com",
        "[secret]",
        "userData",
        "private",
        "ami-0123456789abcdef0",
        "API_KEY",
        "username",
    ] {
        assert!(!sent.contains(never), "the request carries {never:?}: {sent}");
    }
}

#[test]
fn values_known_only_after_apply_are_named_not_sent() {
    let e = estimate(CREATE, &Options::default());
    let web = resource(
        &e,
        "urn:pulumi:dev::shop::shop:index:Api$aws:ec2/instance:Instance::web",
    );
    assert_eq!(web.unknown, ["instanceType"]);
    assert_eq!(
        Value::Object(web.after.clone().unwrap()),
        json!({"rootBlockDevice": {"volumeSize": 50, "volumeType": "gp3"}})
    );
}

#[test]
fn the_region_comes_from_the_provider_then_the_stack() {
    let e = estimate(
        CREATE,
        &Options {
            region: Some("ap-south-1".into()),
            ..Options::default()
        },
    );
    let region = |urn: &str| resource(&e, urn).region.clone();
    assert_eq!(
        region("urn:pulumi:dev::shop::aws:rds/instance:Instance::orders").as_deref(),
        Some("eu-north-1")
    );
    assert_eq!(
        region("urn:pulumi:dev::shop::shop:index:Api$aws:ec2/instance:Instance::web").as_deref(),
        Some("us-east-1")
    );
    assert_eq!(
        region("urn:pulumi:dev::shop::aws:lambda/function:Function::resize").as_deref(),
        Some("eu-west-1"),
        "no provider named: the stack's aws:region, before --region"
    );
}

#[test]
fn region_flag_is_used_when_the_preview_names_none() {
    let without_config = CREATE.replace("\"aws:region\": \"eu-west-1\",", "");
    let e = estimate(
        &without_config,
        &Options {
            region: Some("ap-south-1".into()),
            ..Options::default()
        },
    );
    let lambda = resource(&e, "urn:pulumi:dev::shop::aws:lambda/function:Function::resize");
    assert_eq!(lambda.region.as_deref(), Some("ap-south-1"));

    let e = estimate(&without_config, &Options::default());
    let lambda = resource(&e, "urn:pulumi:dev::shop::aws:lambda/function:Function::resize");
    assert_eq!(lambda.region, None);
}

#[test]
fn updates_replacements_and_deletes_carry_before_and_after() {
    let e = estimate(UPDATE, &Options::default());
    assert_eq!(
        e.request.resources.len(),
        5,
        "the read is left out; the same instance goes, for the stack's cost"
    );
    let web = resource(&e, "urn:pulumi:prod::shop::aws:ec2/instance:Instance::web");
    assert_eq!(web.change, ChangeKind::Same);

    let orders = resource(&e, "urn:pulumi:prod::shop::aws:rds/instance:Instance::orders");
    assert_eq!(orders.change, ChangeKind::Update);
    assert_eq!(orders.before.as_ref().unwrap()["instanceClass"], "db.r6i.xlarge");
    assert_eq!(orders.after.as_ref().unwrap()["instanceClass"], "db.r6i.2xlarge");
    assert_eq!(
        orders.region.as_deref(),
        Some("eu-west-1"),
        "a provider not in the preview: the stack's region"
    );

    let data = resource(&e, "urn:pulumi:prod::shop::aws:ebs/volume:Volume::data");
    assert_eq!(
        data.change,
        ChangeKind::Replace,
        "three replacement steps are one change"
    );
    assert_eq!(
        Value::Object(data.before.clone().unwrap()),
        json!({"size": 100, "type": "gp2"})
    );
    assert_eq!(
        Value::Object(data.after.clone().unwrap()),
        json!({"size": 200, "type": "gp3"})
    );

    let egress = resource(&e, "urn:pulumi:prod::shop::aws:ec2/natGateway:NatGateway::egress");
    assert_eq!(egress.change, ChangeKind::Delete);
    assert!(egress.after.is_none());

    let assets = resource(&e, "urn:pulumi:prod::shop::aws:s3/bucket:Bucket::assets");
    assert_eq!(assets.change, ChangeKind::Update);
    assert!(assets.before.as_ref().unwrap().is_empty() && assets.after.as_ref().unwrap().is_empty());
}

#[test]
fn resource_ids_are_sent_only_when_asked() {
    let e = estimate(UPDATE, &Options::default());
    assert!(e.request.resources.iter().all(|r| r.resource_id.is_none()));
    assert!(!serde_json::to_string(&e.request).unwrap().contains("resourceId"));

    let e = estimate(
        UPDATE,
        &Options {
            with_resource_ids: true,
            ..Options::default()
        },
    );
    let id = |urn: &str| resource(&e, urn).resource_id.clone();
    assert_eq!(
        id("urn:pulumi:prod::shop::aws:rds/instance:Instance::orders").as_deref(),
        Some("orders-db-7f3a")
    );
    assert_eq!(
        id("urn:pulumi:prod::shop::aws:ebs/volume:Volume::data").as_deref(),
        Some("vol-0abc1234def567890")
    );
    assert_eq!(
        id("urn:pulumi:prod::shop::aws:ec2/natGateway:NatGateway::egress").as_deref(),
        Some("nat-0123456789abcdef0")
    );

    let created = estimate(
        CREATE,
        &Options {
            with_resource_ids: true,
            ..Options::default()
        },
    );
    assert!(
        created.request.resources.iter().all(|r| r.resource_id.is_none()),
        "a new resource has no ID yet"
    );
}

#[test]
fn the_comment_names_resources_from_ci_and_figures_from_the_answer() {
    let e = estimate(UPDATE, &Options::default());
    let orders = key("urn:pulumi:prod::shop::aws:rds/instance:Instance::orders");
    let data = key("urn:pulumi:prod::shop::aws:ebs/volume:Volume::data");
    let egress = key("urn:pulumi:prod::shop::aws:ec2/natGateway:NatGateway::egress");
    let assets = key("urn:pulumi:prod::shop::aws:s3/bucket:Bucket::assets");
    let response: Response = serde_json::from_value(json!({
        "formatVersion": 1,
        "estimateId": "est_01",
        "currency": "USD",
        "resources": [
            {"key": orders, "monthlyBefore": 398.0, "monthlyAfter": 810.0, "basis": "company-rate", "notes": [
                {"kind": "saving", "text": "db.r7g.2xlarge does the same job on Graviton.", "monthlySaving": 120.0, "risk": "low"},
                {"kind": "usage", "text": "Its busiest hour in the last 30 days used 41% CPU."}
            ]},
            {"key": data, "monthlyBefore": 11.0, "monthlyAfter": 17.6, "basis": "list-price"},
            {"key": egress, "monthlyBefore": 37.96, "monthlyAfter": null, "basis": "company-rate"},
            {"key": assets, "basis": "not-priced", "reason": "Priced from usage; no usage collected for it yet."}
        ]
    }))
    .unwrap();

    let comment = render::comment(&response, &e);
    // 810 - 398 + 17.6 - 11 + 0 - 37.96 = 380.64
    assert!(
        comment.starts_with("### qloudpulse: this change adds about $381 a month\n"),
        "{comment}"
    );
    assert!(
        comment.contains("| `orders` RDS instance | changed | $398 | $810 | your rates |"),
        "{comment}"
    );
    assert!(
        comment.contains("| `data` EBS volume | replaced | $11 | $18 | list price |"),
        "{comment}"
    );
    assert!(
        comment.contains("| `egress` NAT gateway | removed | $38 | – | your rates |"),
        "{comment}"
    );
    assert!(
        comment
            .contains("- `orders`: db.r7g.2xlarge does the same job on Graviton. About $120 a month less. Risk: low."),
        "{comment}"
    );
    assert!(
        comment.contains("**What it uses today**\n\n- `orders`: Its busiest hour"),
        "{comment}"
    );
    assert!(
        comment.contains("- `assets` S3 bucket: Priced from usage; no usage collected for it yet."),
        "{comment}"
    );
    assert!(comment.contains("Estimate `est_01`."), "{comment}");
}

#[test]
fn a_change_that_saves_says_so() {
    let e = estimate(UPDATE, &Options::default());
    let egress = key("urn:pulumi:prod::shop::aws:ec2/natGateway:NatGateway::egress");
    let response: Response = serde_json::from_value(json!({
        "formatVersion": 1, "currency": "USD",
        "resources": [{"key": egress, "monthlyBefore": 37.96, "basis": "company-rate"}]
    }))
    .unwrap();
    let comment = render::comment(&response, &e);
    assert!(
        comment.starts_with("### qloudpulse: this change saves about $38 a month\n"),
        "{comment}"
    );
    assert!(comment.contains("`orders` RDS instance: no answer for it"), "{comment}");
}

fn run(args: &[&str], stdin: &str, token: Option<&str>) -> (bool, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_qloudpulse"));
    cmd.args(args)
        .env_remove("QLOUDPULSE_TOKEN")
        .env_remove("QLOUDPULSE_ENDPOINT")
        // The shipped list, so the tests reach no network; a test can still name another with --catalogue.
        .env("QLOUDPULSE_CATALOGUE", CATALOGUE_PATH)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(t) = token {
        cmd.env("QLOUDPULSE_TOKEN", t);
    }
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn dry_run_prints_the_request_and_needs_no_token() {
    let (ok, stdout, stderr) = run(&["estimate", "--dry-run"], UPDATE, None);
    assert!(ok, "{stderr}");
    let request: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(request["formatVersion"], 1);
    assert_eq!(request["tool"], "pulumi");
    assert_eq!(request["resources"].as_array().unwrap().len(), 5);
    assert_eq!(
        stderr.trim(),
        "qloudpulse: 4 resources to price, 0 not priced; and 1 unchanged, for the stack's cost, 0 not priced"
    );

    let without_sames = UPDATE.replace("\"op\": \"same\"", "\"op\": \"read\"");
    let (ok, _, stderr) = run(&["estimate", "--dry-run"], &without_sames, None);
    assert!(ok, "{stderr}");
    assert_eq!(stderr.trim(), "qloudpulse: 4 resources to price, 0 not priced");
}

#[test]
fn reads_a_file_as_well_as_stdin() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/preview-create.json");
    let (ok, stdout, stderr) = run(&["estimate", "--dry-run", path], "", None);
    assert!(ok, "{stderr}");
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["resources"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn sending_needs_a_token() {
    let (ok, _, stderr) = run(&["estimate"], UPDATE, None);
    assert!(!ok);
    assert!(stderr.contains("set QLOUDPULSE_TOKEN"), "{stderr}");
}

#[test]
fn the_token_is_never_sent_over_plain_http() {
    let (ok, _, stderr) = run(
        &["estimate", "--endpoint", "http://app.qloudpulse.com/api"],
        UPDATE,
        Some("qpev_secret_token"),
    );
    assert!(!ok);
    assert!(stderr.contains("must start with https://"), "{stderr}");
    assert!(!stderr.contains("qpev_secret_token"));
}

#[test]
fn nothing_to_price_needs_no_call() {
    // The endpoint is a closed port, so a call would fail the run.
    let (ok, stdout, stderr) = run(
        &["estimate", "--endpoint", "https://127.0.0.1:9/v1/estimates"],
        r#"{"steps": []}"#,
        Some("qpev_test"),
    );
    assert!(ok, "{stderr}");
    assert!(
        stdout.starts_with("### qloudpulse: nothing in this change could be priced"),
        "{stdout}"
    );
}

#[test]
fn nothing_to_price_still_needs_a_token() {
    let (ok, stdout, stderr) = run(&["estimate"], r#"{"steps": []}"#, None);
    assert!(!ok, "{stdout}");
    assert!(stdout.is_empty(), "{stdout}");
    assert_eq!(
        stderr.trim(),
        "qloudpulse: set QLOUDPULSE_TOKEN to a change-event source's token (qpev_...), from Settings, Events in \
         qloudpulse, or use --dry-run"
    );

    let (ok, _, stderr) = run(&["estimate"], r#"{"steps": []}"#, Some("   "));
    assert!(
        !ok && stderr.contains("set QLOUDPULSE_TOKEN"),
        "a blank token is none: {stderr}"
    );

    let (ok, stdout, stderr) = run(&["estimate", "--dry-run"], r#"{"steps": []}"#, None);
    assert!(ok, "{stderr}");
    assert_eq!(serde_json::from_str::<Value>(&stdout).unwrap()["resources"], json!([]));
}

#[test]
fn a_missing_token_is_said_before_the_preview_is_read_or_the_catalogue_asked() {
    let out = Command::new(env!("CARGO_BIN_EXE_qloudpulse"))
        // A closed port and no catalogue file: reaching for the catalogue would fail with a connection error instead.
        .args([
            "estimate",
            "--endpoint",
            "https://127.0.0.1:9/v1/estimates",
            "/nonexistent/preview.json",
        ])
        .env_remove("QLOUDPULSE_TOKEN")
        .env_remove("QLOUDPULSE_CATALOGUE")
        .env_remove("QLOUDPULSE_ENDPOINT")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("set QLOUDPULSE_TOKEN"), "{err}");
    assert!(!err.contains("127.0.0.1") && !err.contains("could not read"), "{err}");
}

#[test]
fn input_that_is_not_a_preview_is_refused() {
    let (ok, _, stderr) = run(&["estimate", "--dry-run"], "Previewing update (dev)", None);
    assert!(!ok);
    assert!(stderr.contains("not the JSON of `pulumi preview --json`"), "{stderr}");
}

#[test]
fn region_flag_must_be_a_region() {
    let (ok, _, stderr) = run(&["estimate", "--dry-run", "--region", "europe"], UPDATE, None);
    assert!(!ok);
    assert!(stderr.contains("--region takes an AWS region name"), "{stderr}");
}

/// A service and its task definition, as Pulumi writes them: the service's `taskDefinition` comes from the task
/// definition, which `propertyDependencies` names.
const ECS_CATALOGUE: &str = r#"{"formatVersion": 1, "providers": ["aws"], "types": [
    {"type": "aws:ecs/service:Service", "label": "ECS service", "attributes": ["desiredCount", "launchType"],
     "references": ["taskDefinition"]},
    {"type": "aws:ecs/taskDefinition:TaskDefinition", "label": "ECS task definition",
     "attributes": ["cpu", "memory", "runtimePlatform.cpuArchitecture"]}]}"#;

fn ecs_preview(task_op: &str) -> String {
    let task = "urn:pulumi:prod::shop::aws:ecs/taskDefinition:TaskDefinition::api";
    let state = |count: u32| {
        json!({"type": "aws:ecs/service:Service", "custom": true, "id": "arn:aws:ecs:eu-west-1:1:service/c/api",
               "inputs": {"desiredCount": count, "launchType": "FARGATE", "name": "api",
                          "taskDefinition": "arn:aws:ecs:eu-west-1:1:task-definition/api:7"},
               "propertyDependencies": {"taskDefinition": [task], "cluster": ["urn:pulumi:prod::shop::aws:ecs/cluster:Cluster::c"]}})
    };
    let task_state = json!({"type": "aws:ecs/taskDefinition:TaskDefinition", "custom": true,
        "inputs": {"cpu": "512", "memory": "1024", "family": "api",
                   "containerDefinitions": "[{\"environment\":[{\"name\":\"DB_PASSWORD\",\"value\":\"hunter2\"}]}]",
                   "runtimePlatform": {"cpuArchitecture": "ARM64", "operatingSystemFamily": "LINUX"}}});
    json!({"config": {"aws:region": "eu-west-1"}, "steps": [
        {"op": "update", "urn": "urn:pulumi:prod::shop::aws:ecs/service:Service::api",
         "oldState": state(2), "newState": state(4)},
        {"op": task_op, "urn": task, "oldState": task_state, "newState": task_state}
    ]})
    .to_string()
}

#[test]
fn a_reference_names_the_resource_by_key_and_an_unchanged_one_goes_too() {
    let e = estimate_with(&ecs_preview("same"), ECS_CATALOGUE, &Options::default());
    let task = "urn:pulumi:prod::shop::aws:ecs/taskDefinition:TaskDefinition::api";

    let service = resource(&e, "urn:pulumi:prod::shop::aws:ecs/service:Service::api");
    assert_eq!(service.change, ChangeKind::Update);
    assert_eq!(
        service.references.as_ref().unwrap().get("taskDefinition"),
        Some(&key(task))
    );
    assert_eq!(
        service.references.as_ref().unwrap().len(),
        1,
        "only the catalogue's references"
    );
    assert_eq!(
        Value::Object(service.after.clone().unwrap()),
        json!({"desiredCount": 4, "launchType": "FARGATE"})
    );

    let definition = resource(&e, task);
    assert_eq!(definition.change, ChangeKind::Same);
    assert!(definition.before.is_none());
    assert_eq!(
        Value::Object(definition.after.clone().unwrap()),
        json!({"cpu": "512", "memory": "1024", "runtimePlatform": {"cpuArchitecture": "ARM64"}})
    );
    // An unchanged resource keeps its name in CI, and nothing else of it is sent.
    assert_eq!(e.locals.len(), 2);
    assert_eq!(e.locals[1].change, ChangeKind::Same);
    assert_eq!(e.locals[1].name, "api");
    let sent = serde_json::to_string(&e.request).unwrap();
    assert!(
        !sent.contains("hunter2") && !sent.contains("DB_PASSWORD") && !sent.contains("arn:aws"),
        "{sent}"
    );
}

#[test]
fn a_referenced_resource_that_changes_is_sent_once() {
    let e = estimate_with(&ecs_preview("update"), ECS_CATALOGUE, &Options::default());
    assert_eq!(e.request.resources.len(), 2);
    assert!(e.request.resources.iter().all(|r| r.change == ChangeKind::Update));
    assert_eq!(e.locals.len(), 2);
}

#[test]
fn the_shipped_catalogue_sends_a_service_with_its_task_definition() {
    let e = estimate(&ecs_preview("same"), &Options::default());
    assert!(e.request.not_priced.is_empty(), "{:?}", e.request.not_priced);
    let kinds: Vec<ChangeKind> = e.request.resources.iter().map(|r| r.change).collect();
    assert_eq!(kinds, [ChangeKind::Update, ChangeKind::Same]);
    let sent = serde_json::to_string(&e.request).unwrap();
    assert!(
        !sent.contains("hunter2") && !sent.contains("containerDefinitions"),
        "{sent}"
    );
}

#[test]
fn without_a_catalogue_file_it_is_read_from_the_endpoint() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_qloudpulse"));
    let out = cmd
        .args([
            "estimate",
            "--dry-run",
            "--endpoint",
            "http://127.0.0.1:9/api/report/public/estimates",
        ])
        .env_remove("QLOUDPULSE_CATALOGUE")
        .env_remove("QLOUDPULSE_ENDPOINT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    out.stdin.as_ref().unwrap().write_all(UPDATE.as_bytes()).unwrap();
    let out = out.wait_with_output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains("http://127.0.0.1:9/api/report/public/estimates/catalogue"),
        "{err}"
    );
}

fn catalogue_with_client(latest: &str, minimum: &str) -> String {
    let mut c: Value = serde_json::from_str(CATALOGUE).unwrap();
    c["client"] = json!({"latest": latest, "minimum": minimum, "download": "https://example.com/get"});
    let path = std::env::temp_dir().join(format!(
        "qloudpulse-catalogue-{latest}-{minimum}-{}.json",
        std::process::id()
    ));
    std::fs::write(&path, c.to_string()).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
fn a_newer_client_is_said_and_one_below_the_minimum_stops() {
    let newer = catalogue_with_client("99.0.0", "0.0.1");
    let (ok, stdout, stderr) = run(&["estimate", "--dry-run", "--catalogue", &newer], UPDATE, None);
    assert!(ok, "{stderr}");
    assert!(stderr.contains("qloudpulse CLI 99.0.0 is out (this is "), "{stderr}");
    assert!(
        serde_json::from_str::<Value>(&stdout).is_ok(),
        "the request is still printed"
    );

    let too_old = catalogue_with_client("99.0.0", "98.0.0");
    let (ok, stdout, stderr) = run(&["estimate", "--dry-run", "--catalogue", &too_old], UPDATE, None);
    assert!(!ok);
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("qloudpulse takes 98.0.0 or later. Download 99.0.0 from https://example.com/get"),
        "{stderr}"
    );

    let current = catalogue_with_client(qloudpulse_cli::version::VERSION, qloudpulse_cli::version::VERSION);
    let (ok, _, stderr) = run(&["estimate", "--dry-run", "--catalogue", &current], UPDATE, None);
    assert!(ok && !stderr.contains("is out"), "{stderr}");
}

#[test]
fn the_types_asked_about_are_every_cloud_resource_in_the_preview_once() {
    let preview = pulumi::parse(&ecs_preview("same")).unwrap();
    assert_eq!(
        pulumi::found_types(&preview),
        ["aws:ecs/service:Service", "aws:ecs/taskDefinition:TaskDefinition"]
    );
    let update = pulumi::found_types(&pulumi::parse(UPDATE).unwrap());
    assert!(update.iter().all(|t| !t.starts_with("pulumi:")), "{update:?}");
    assert!(update.contains(&"aws:rds/instance:Instance".to_string()), "{update:?}");
}

/// The update fixture's answer from an endpoint that prices unchanged resources: the changed ones as in
/// `the_comment_names_resources_from_ci_and_figures_from_the_answer`, and the unchanged instance at one figure.
fn stack_response(with_sames: bool) -> Response {
    let orders = key("urn:pulumi:prod::shop::aws:rds/instance:Instance::orders");
    let data = key("urn:pulumi:prod::shop::aws:ebs/volume:Volume::data");
    let egress = key("urn:pulumi:prod::shop::aws:ec2/natGateway:NatGateway::egress");
    let assets = key("urn:pulumi:prod::shop::aws:s3/bucket:Bucket::assets");
    let web = key("urn:pulumi:prod::shop::aws:ec2/instance:Instance::web");
    let mut resources = vec![
        json!({"key": orders, "monthlyBefore": 398.0, "monthlyAfter": 810.0, "basis": "company-rate"}),
        json!({"key": data, "monthlyBefore": 11.0, "monthlyAfter": 17.6, "basis": "list-price"}),
        json!({"key": egress, "monthlyBefore": 37.96, "monthlyAfter": null, "basis": "company-rate"}),
        json!({"key": assets, "basis": "not-priced", "reason": "Priced from usage; no usage collected for it yet."}),
    ];
    if with_sames {
        resources.push(json!({"key": web, "monthlyBefore": 70.08, "monthlyAfter": 70.08, "basis": "list-price"}));
    }
    serde_json::from_value(
        json!({"formatVersion": 1, "estimateId": "est_02", "currency": "USD", "resources": resources}),
    )
    .unwrap()
}

/// A stack of the update fixture with an unchanged SQS queue too: of the catalogue's provider, and not priced.
fn update_with_an_unchanged_queue() -> String {
    let mut preview: Value = serde_json::from_str(UPDATE).unwrap();
    let queue = json!({"urn": "urn:pulumi:prod::shop::aws:sqs/queue:Queue::jobs", "custom": true,
                       "id": "https://sqs.eu-west-1.amazonaws.com/1/jobs", "type": "aws:sqs/queue:Queue",
                       "inputs": {"name": "jobs"}});
    preview["steps"].as_array_mut().unwrap().push(json!({
        "op": "same", "urn": "urn:pulumi:prod::shop::aws:sqs/queue:Queue::jobs", "oldState": queue, "newState": queue
    }));
    preview.to_string()
}

#[test]
fn every_unchanged_priced_resource_is_sent_with_its_name_kept_in_ci() {
    let e = estimate(
        &update_with_an_unchanged_queue(),
        &Options {
            with_resource_ids: true,
            ..Options::default()
        },
    );
    assert!(e.stack_listed);
    assert_eq!(e.stack_too_large, None);
    assert_eq!(e.stack_not_priced, 1, "the queue, counted but not sent");
    assert!(
        e.request.not_priced.is_empty(),
        "notPriced is the change's: {:?}",
        e.request.not_priced
    );

    let web = resource(&e, "urn:pulumi:prod::shop::aws:ec2/instance:Instance::web");
    assert_eq!(web.change, ChangeKind::Same);
    assert_eq!(web.region.as_deref(), Some("eu-west-1"));
    assert!(web.before.is_none());
    assert_eq!(
        Value::Object(web.after.clone().unwrap()),
        json!({"instanceType": "m5.large"})
    );
    assert!(web.resource_id.is_none(), "never the cloud ID of an unchanged resource");

    let local = e.locals.iter().find(|l| l.key == web.key).unwrap();
    assert_eq!((local.name.as_str(), local.change), ("web", ChangeKind::Same));
    assert_eq!(local.label, "EC2 instance");

    let sent = serde_json::to_string(&e.request).unwrap();
    for never in ["i-0abc", "web", "jobs", "sqs.eu-west-1", "urn:"] {
        assert!(!sent.contains(never), "the request carries {never:?}: {sent}");
    }
}

#[test]
fn the_comment_gives_the_stack_beside_the_change() {
    let e = estimate(&update_with_an_unchanged_queue(), &Options::default());
    let comment = render::comment(&stack_response(true), &e);
    // The change: 810 - 398 + 17.6 - 11 - 37.96 = 380.64. The stack now: 398 + 11 + 37.96 + 70.08 = 517.04; after:
    // 810 + 17.6 + 0 + 70.08 = 897.68. Priced: orders, data, egress, web. Not priced: assets and the queue.
    assert!(
        comment.starts_with(
            "### qloudpulse: this change adds about $381 a month\n\n**The stack:** about $517 a month now, about $898 \
             after this change, from 4 priced resources; 2 not priced.\n\n| Resource"
        ),
        "{comment}"
    );
    assert!(
        !comment.contains("`web`"),
        "an unchanged resource gets no line of its own: {comment}"
    );
    assert!(!comment.contains("show-sames"), "{comment}");
}

#[test]
fn a_preview_without_sames_says_how_to_get_the_stack() {
    let without_sames = UPDATE.replace("\"op\": \"same\"", "\"op\": \"read\"");
    let e = estimate(&without_sames, &Options::default());
    assert!(!e.stack_listed);
    assert!(e.locals.iter().all(|l| l.change != ChangeKind::Same));
    let comment = render::comment(&stack_response(false), &e);
    assert!(!comment.contains("**The stack:**"), "{comment}");
    assert!(
        comment.contains(
            "<sub>Run `pulumi preview --json --show-sames` to see the whole stack's monthly cost.</sub>\n\n<sub>Monthly"
        ),
        "{comment}"
    );
}

#[test]
fn an_endpoint_that_leaves_sames_unanswered_leaves_the_stack_out() {
    let e = estimate(UPDATE, &Options::default());
    let comment = render::comment(&stack_response(false), &e);
    assert!(
        comment.starts_with(
            "### qloudpulse: this change adds about $381 a month\n\n**The stack:** its cost is left out: this \
             endpoint does not price unchanged resources yet.\n\n"
        ),
        "{comment}"
    );
    assert!(!comment.contains("about $517"), "{comment}");
    assert!(
        !comment.contains("`web` EC2 instance: no answer"),
        "an unanswered same is not a resource of the change left unpriced: {comment}"
    );
}

#[test]
fn a_stack_past_the_request_limit_sends_the_referenced_sames_only() {
    let mut preview: Value = serde_json::from_str(&ecs_preview("same")).unwrap();
    let steps = preview["steps"].as_array_mut().unwrap();
    for i in 0..5000 {
        let urn = format!("urn:pulumi:prod::shop::aws:ebs/volume:Volume::v{i}");
        let state = json!({"type": "aws:ebs/volume:Volume", "custom": true, "id": format!("vol-{i}"),
                           "inputs": {"size": 10, "type": "gp3"}});
        steps.push(json!({"op": "same", "urn": urn, "oldState": state, "newState": state}));
    }
    let e = estimate(&preview.to_string(), &Options::default());
    assert_eq!(
        e.stack_too_large,
        Some(5002),
        "the service, its task definition and 5000 volumes"
    );
    let kinds: Vec<ChangeKind> = e.request.resources.iter().map(|r| r.change).collect();
    assert_eq!(
        kinds,
        [ChangeKind::Update, ChangeKind::Same],
        "the task definition still goes"
    );

    let service = key("urn:pulumi:prod::shop::aws:ecs/service:Service::api");
    let response: Response = serde_json::from_value(json!({"formatVersion": 1, "currency": "USD", "resources": [
        {"key": service, "monthlyBefore": 36.0, "monthlyAfter": 72.0, "basis": "list-price"}]}))
    .unwrap();
    let comment = render::comment(&response, &e);
    assert!(
        comment.contains(
            "**The stack:** its cost is left out: with every unchanged resource the request would hold 5,002 \
             resources, and one estimate takes 5,000 at most."
        ),
        "{comment}"
    );
    assert!(!comment.contains("show-sames"), "{comment}");

    // One fewer volume fits, and every one goes.
    let mut preview: Value = serde_json::from_str(&preview.to_string()).unwrap();
    preview["steps"].as_array_mut().unwrap().pop();
    preview["steps"].as_array_mut().unwrap().pop();
    let e = estimate(&preview.to_string(), &Options::default());
    assert_eq!(e.stack_too_large, None);
    assert_eq!(e.request.resources.len(), 5000);
}
