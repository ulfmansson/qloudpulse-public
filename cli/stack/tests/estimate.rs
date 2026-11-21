//! The estimate command against Pulumi previews written to Pulumi's `display.PreviewDigest` format.

use std::io::Write as _;
use std::process::{Command, Stdio};

use qloudpulse_stack::catalogue::Catalogue;
use qloudpulse_stack::pulumi::{self, Options, key};
use qloudpulse_stack::render;
use qloudpulse_stack::request::{ChangeKind, Response};
use serde_json::{Value, json};

const CREATE: &str = include_str!("fixtures/preview-create.json");
const UPDATE: &str = include_str!("fixtures/preview-update.json");

const CATALOGUE: &str = include_str!("fixtures/catalogue.json");
const CATALOGUE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/catalogue.json");
/// A credentials file that does not exist, so a run finds no token kept by `qloudpulse-stack login` and never reads this
/// machine's keychain.
const NO_CREDENTIALS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/no-such-credentials");

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

fn resource<'a>(e: &'a pulumi::Estimate, urn: &str) -> &'a qloudpulse_stack::request::ResourceChange {
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

    // The project and stack are named, as `stack` (qloudpulse's ADR 0133); nothing else of the URNs goes.
    assert_eq!(e.request.stack.as_deref(), Some("shop/dev"));
    let mut without_stack = e.request.clone();
    without_stack.stack = None;
    let sent = serde_json::to_string(&without_stack).unwrap();
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
fn the_region_comes_from_the_provider_then_region_flag_then_the_stack() {
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
        Some("ap-south-1"),
        "no provider named: --region, before the stack's aws:region"
    );

    let e = estimate(CREATE, &Options::default());
    assert_eq!(
        resource(&e, "urn:pulumi:dev::shop::aws:lambda/function:Function::resize")
            .region
            .as_deref(),
        Some("eu-west-1"),
        "without --region: the stack's aws:region"
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

/// The region a dry run sends for the Lambda function of the new stack, whose provider names none.
fn lambda_region(stdout: &str) -> Value {
    let request: Value = serde_json::from_str(stdout).unwrap();
    let k = key("urn:pulumi:dev::shop::aws:lambda/function:Function::resize");
    request["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == k)
        .unwrap()["region"]
        .clone()
}

#[test]
fn the_machines_aws_region_and_profile_are_read_when_the_preview_names_none() {
    let without_config = CREATE.replace("\"aws:region\": \"eu-west-1\",", "");

    let (ok, stdout, stderr) = run_env(
        &["estimate", "--dry-run"],
        &without_config,
        None,
        &[("AWS_REGION", "eu-central-1"), ("AWS_DEFAULT_REGION", "us-west-2")],
    );
    assert!(ok, "{stderr}");
    assert_eq!(lambda_region(&stdout), "eu-central-1");
    assert!(!stderr.contains("no AWS region"), "{stderr}");

    let config = std::env::temp_dir().join(format!("qloudpulse-aws-config-{}", std::process::id()));
    std::fs::write(
        &config,
        "[default]\nregion = us-east-1\n[profile ci]\nregion = sa-east-1\naws_secret_access_key = never-sent\n",
    )
    .unwrap();
    let config_path = config.to_str().unwrap();
    let (ok, stdout, stderr) = run_env(
        &["estimate", "--dry-run"],
        &without_config,
        None,
        &[("AWS_CONFIG_FILE", config_path), ("AWS_PROFILE", "ci")],
    );
    assert!(ok, "{stderr}");
    assert_eq!(lambda_region(&stdout), "sa-east-1");
    assert!(!stdout.contains("never-sent"));

    let (ok, stdout, _) = run_env(
        &["estimate", "--dry-run", "--region", "ap-south-1"],
        &without_config,
        None,
        &[("AWS_CONFIG_FILE", config_path), ("AWS_REGION", "eu-central-1")],
    );
    assert!(ok);
    assert_eq!(lambda_region(&stdout), "ap-south-1", "--region wins over this machine");
    std::fs::remove_file(&config).unwrap();
}

#[test]
fn a_resource_left_without_a_region_is_said_on_stderr_and_in_the_comment() {
    let without_config = CREATE.replace("\"aws:region\": \"eu-west-1\",", "");
    let (ok, stdout, stderr) = run(&["estimate", "--dry-run"], &without_config, None);
    assert!(ok, "{stderr}");
    assert_eq!(lambda_region(&stdout), Value::Null);
    assert!(
        stderr.contains(
            "qloudpulse: 1 resource has no AWS region, so qloudpulse cannot price it: neither the preview, its \
             provider, the stack's config nor this machine's AWS settings name one. Set one with pulumi config set \
             aws:region <region>, or run with --region <region>."
        ),
        "{stderr}"
    );

    let e = estimate(&without_config, &Options::default());
    assert_eq!(e.no_region, 1);
    let response: Response =
        serde_json::from_value(json!({"formatVersion": 1, "currency": "USD", "resources": []})).unwrap();
    let comment = render::comment(&response, &e);
    assert!(
        comment.contains("**No region:** 1 resource has no AWS region"),
        "{comment}"
    );
    assert!(
        comment.contains("`pulumi config set aws:region <region>`, or run with `--region <region>`."),
        "{comment}"
    );

    let e = estimate(CREATE, &Options::default());
    assert!(!render::comment(&response, &e).contains("No region"));
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
    run_env(args, stdin, token, &[])
}

/// A run with these environment variables set besides.
fn run_env(args: &[&str], stdin: &str, token: Option<&str>, env: &[(&str, &str)]) -> (bool, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_qloudpulse-stack"));
    cmd.args(args)
        .env_remove("QLOUDPULSE_TOKEN")
        .env_remove("QLOUDPULSE_ENDPOINT")
        // Never this machine's keychain: a file that is not there, so nothing is kept.
        .env("QLOUDPULSE_CREDENTIALS_FILE", NO_CREDENTIALS)
        // The shipped list, so the tests reach no network; a test can still name another with --catalogue.
        .env("QLOUDPULSE_CATALOGUE", CATALOGUE_PATH)
        // Never this machine's AWS settings, so a region comes from the preview and the options only.
        .env_remove("AWS_REGION")
        .env_remove("AWS_DEFAULT_REGION")
        .env_remove("AWS_PROFILE")
        .env("AWS_CONFIG_FILE", NO_CREDENTIALS)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(t) = token {
        cmd.env("QLOUDPULSE_TOKEN", t);
    }
    cmd.envs(env.iter().copied());
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
        "qloudpulse: set QLOUDPULSE_TOKEN to a token of your own (qpev_...). Make one at \
         https://app.qloudpulse.com/ui/?profile#my-tokens (Your tokens, under your name), or run `qloudpulse-stack login` at \
         a terminal to sign in with your browser, or use --dry-run to see what would be sent.",
        "the default endpoint is hosted qloudpulse: the page is linked"
    );

    let (ok, _, stderr) = run(
        &["estimate", "--endpoint", "https://qloudpulse.example.com/v1/estimates"],
        r#"{"steps": []}"#,
        None,
    );
    assert!(!ok);
    assert_eq!(
        stderr.trim(),
        "qloudpulse: set QLOUDPULSE_TOKEN to a token of your own (qpev_...). Make one under Your tokens, under your \
         name in your qloudpulse, or run `qloudpulse-stack login` at a terminal to sign in with your browser, or use \
         --dry-run to see what would be sent.",
        "an install of its own: no address guessed"
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
    let out = Command::new(env!("CARGO_BIN_EXE_qloudpulse-stack"))
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
        .env("QLOUDPULSE_CREDENTIALS_FILE", NO_CREDENTIALS)
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
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_qloudpulse-stack"));
    let out = cmd
        .args([
            "estimate",
            "--dry-run",
            "--endpoint",
            "http://127.0.0.1:9/api/report/public/estimates",
        ])
        .env_remove("QLOUDPULSE_CATALOGUE")
        .env_remove("QLOUDPULSE_ENDPOINT")
        .env("QLOUDPULSE_CREDENTIALS_FILE", NO_CREDENTIALS)
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
    assert!(stderr.contains("qloudpulse-stack 99.0.0 is out (this is "), "{stderr}");
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

    let current = catalogue_with_client(qloudpulse_stack::version::VERSION, qloudpulse_stack::version::VERSION);
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

const UNCHANGED: &str = include_str!("fixtures/preview-unchanged.json");

/// The answer of `stack_response(true)` with a saving note on `orders`, rendered both ways below.
fn noted_response() -> Response {
    let mut r = stack_response(true);
    let orders = key("urn:pulumi:prod::shop::aws:rds/instance:Instance::orders");
    r.resources[0] = serde_json::from_value(json!({
        "key": orders, "monthlyBefore": 398.0, "monthlyAfter": 810.0, "basis": "company-rate",
        "notes": [{"kind": "saving", "text": "db.r7g.2xlarge does the same job on Graviton.", "monthlySaving": 120.0,
                   "risk": "low"}]
    }))
    .unwrap();
    r
}

#[test]
fn the_stack_and_its_providers_alone_are_no_stack_listed() {
    // What the user ran: `pulumi preview --json | qloudpulse-stack estimate`, no --show-sames, nothing changed. Pulumi
    // still lists the stack and its provider as the same.
    let e = estimate(UNCHANGED, &Options::default());
    assert!(!e.stack_listed);
    let (ok, stdout, stderr) = run(
        &["estimate", "--endpoint", "https://127.0.0.1:9/v1/estimates"],
        UNCHANGED,
        Some("qpev_test"),
    );
    assert!(ok, "{stderr}");
    assert_eq!(stderr.trim(), "qloudpulse: 0 resources to price, 0 not priced");
    assert_eq!(
        stdout,
        "### qloudpulse: nothing in this change could be priced\n\n<sub>Run `pulumi preview --json --show-sames` to \
         see the whole stack's monthly cost.</sub>\n\n<sub>Monthly figures in USD, before tax.</sub>\n",
        "stdout is a pipe: Markdown, and the hint rather than a stack line"
    );
}

#[test]
fn a_stack_listed_but_not_priced_says_so() {
    let e = estimate(UPDATE, &Options::default());
    assert!(e.stack_listed);
    let resources: Vec<Value> = e
        .request
        .resources
        .iter()
        .map(|r| json!({"key": r.key, "basis": "not-priced"}))
        .collect();
    let response: Response =
        serde_json::from_value(json!({"formatVersion": 1, "currency": "USD", "resources": resources})).unwrap();
    let comment = render::comment(&response, &e);
    assert!(
        comment.starts_with(
            "### qloudpulse: nothing in this change could be priced\n\n**The stack:** none of its resources could be \
             priced; 5 not priced.\n\n"
        ),
        "{comment}"
    );
    assert!(!comment.contains("show-sames"), "{comment}");
}

#[test]
fn the_same_answer_as_markdown_and_as_text() {
    let e = estimate(&update_with_an_unchanged_queue(), &Options::default());
    let response = noted_response();
    assert_eq!(
        render::render(&response, &e, render::Style::Markdown),
        "### qloudpulse: this change adds about $381 a month

**The stack:** about $517 a month now, about $898 after this change, from 4 priced resources; 2 not priced.

| Resource | Change | Monthly now | Monthly after | Priced from |
| --- | --- | --- | --- | --- |
| `orders` RDS instance | changed | $398 | $810 | your rates |
| `data` EBS volume | replaced | $11 | $18 | list price |
| `egress` NAT gateway | removed | $38 | – | your rates |

**Savings to consider**

- `orders`: db.r7g.2xlarge does the same job on Graviton. About $120 a month less. Risk: low.

**Not priced**

- `assets` S3 bucket: Priced from usage; no usage collected for it yet.

<sub>Monthly figures in USD, before tax. Estimate `est_02`.</sub>
"
    );
    assert_eq!(
        render::render(&response, &e, render::Style::Text),
        "qloudpulse: this change adds about $381 a month

The stack: about $517 a month now, about $898 after this change, from 4 priced resources; 2 not priced.

Resource             Change    Monthly now  Monthly after  Priced from
orders RDS instance  changed          $398           $810  your rates
data EBS volume      replaced          $11            $18  list price
egress NAT gateway   removed           $38              –  your rates

Savings to consider:
- orders: db.r7g.2xlarge does the same job on Graviton. About $120 a month less. Risk: low.

Not priced:
- assets S3 bucket: Priced from usage; no usage collected for it yet.

Monthly figures in USD, before tax. Estimate est_02.
"
    );
}

#[test]
fn text_has_no_markup_and_names_stay_clean() {
    let e = estimate(
        &UPDATE.replace("::orders\"", "::ord\\u001b[31mers`|\""),
        &Options::default(),
    );
    let orders = key("urn:pulumi:prod::shop::aws:rds/instance:Instance::ord\u{1b}[31mers`|");
    let response: Response = serde_json::from_value(json!({"formatVersion": 1, "currency": "USD", "resources": [
        {"key": orders, "monthlyBefore": 398.0, "monthlyAfter": 810.0, "basis": "company-rate",
         "notes": [{"kind": "info", "text": "line\u{7}one"}]}]}))
    .unwrap();
    let text = render::render(&response, &e, render::Style::Text);
    for markup in ["###", "**", "<sub>", "| ---"] {
        assert!(!text.contains(markup), "{markup:?} in {text}");
    }
    assert!(!text.chars().any(|c| c.is_control() && c != '\n'), "{text:?}");
    assert!(text.contains("ord[31mers`| RDS instance"), "{text}");
    assert!(text.contains("- ord[31mers`|: lineone"), "{text}");
}

#[test]
fn format_text_and_the_markdown_default_through_the_binary() {
    let args = ["estimate", "--endpoint", "https://127.0.0.1:9/v1/estimates"];
    let (ok, stdout, stderr) = run(
        &[&args[..], &["--format", "text"]].concat(),
        UNCHANGED,
        Some("qpev_test"),
    );
    assert!(ok, "{stderr}");
    assert_eq!(
        stdout,
        "qloudpulse: nothing in this change could be priced\n\nRun pulumi preview --json --show-sames to see the \
         whole stack's monthly cost.\n\nMonthly figures in USD, before tax.\n"
    );
    let (ok, stdout, _) = run(&args, UNCHANGED, Some("qpev_test"));
    assert!(ok && stdout.starts_with("### qloudpulse: "), "{stdout}");
}

#[test]
fn the_stack_is_named_when_every_urn_agrees() {
    let e = estimate(UPDATE, &Options::default());
    assert_eq!(e.request.stack.as_deref(), Some("shop/prod"));
    let (ok, stdout, stderr) = run(&["estimate", "--dry-run"], UPDATE, None);
    assert!(ok, "{stderr}");
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["stack"],
        "shop/prod",
        "--dry-run shows it"
    );

    let mixed = UPDATE.replacen("urn:pulumi:prod::shop::", "urn:pulumi:staging::shop::", 1);
    assert_eq!(
        estimate(&mixed, &Options::default()).request.stack,
        None,
        "two stacks: none named"
    );
    let other_project = UPDATE.replacen("urn:pulumi:prod::shop::", "urn:pulumi:prod::billing::", 1);
    assert_eq!(estimate(&other_project, &Options::default()).request.stack, None);

    let long = UPDATE.replace(
        "urn:pulumi:prod::shop::",
        &format!("urn:pulumi:prod::{}::", "p".repeat(200)),
    );
    assert_eq!(
        estimate(&long, &Options::default()).request.stack,
        None,
        "past 200 characters"
    );
    let fits = UPDATE.replace(
        "urn:pulumi:prod::shop::",
        &format!("urn:pulumi:prod::{}::", "p".repeat(195)),
    );
    assert_eq!(
        estimate(&fits, &Options::default())
            .request
            .stack
            .unwrap()
            .chars()
            .count(),
        200
    );
    let control = UPDATE.replace("urn:pulumi:prod::shop::", "urn:pulumi:prod::sh\\top::");
    assert_eq!(estimate(&control, &Options::default()).request.stack, None);

    let empty = estimate(r#"{"steps": []}"#, &Options::default());
    assert_eq!(empty.request.stack, None);
    assert!(
        !serde_json::to_string(&empty.request).unwrap().contains("stack"),
        "no stack, no field"
    );
}

#[test]
fn the_comment_links_the_estimate_s_page_with_the_names_after_the_hash() {
    let e = estimate(&update_with_an_unchanged_queue(), &Options::default());
    let mut response = noted_response();
    response.url = Some("https://app.qloudpulse.com/ui/?estimate=est_02".into());
    let link = qloudpulse_stack::link::page_link(response.url.as_deref(), &e.locals).unwrap();
    assert!(
        link.starts_with("https://app.qloudpulse.com/ui/?estimate=est_02#names="),
        "{link}"
    );

    let markdown = render::render(&response, &e, render::Style::Markdown);
    assert!(
        markdown.starts_with(&format!(
            "### qloudpulse: this change adds about $381 a month\n\n[See it in qloudpulse]({link})\n\n**The stack:**"
        )),
        "{markdown}"
    );
    assert!(
        markdown.ends_with(
            "<sub>Monthly figures in USD, before tax. Estimate `est_02`.</sub>\n<sub>The resource names in the link \
             stay in your browser: they come after its #, which a browser never sends to a server.</sub>\n"
        ),
        "{markdown}"
    );

    let text = render::render(&response, &e, render::Style::Text);
    assert!(
        text.starts_with(&format!(
            "qloudpulse: this change adds about $381 a month\n\nSee it in qloudpulse: {link}\n\nThe stack:"
        )),
        "{text}"
    );
    assert!(!text.contains("<sub>") && !text.contains("]("), "{text}");

    response.url = None;
    let without = render::render(&response, &e, render::Style::Markdown);
    assert!(
        !without.contains("See it in qloudpulse") && !without.contains("stay in your browser"),
        "{without}"
    );
}

/// The shipped catalogue with `free`, as an endpoint from 2026-11-19 answers: IAM roles and S3 Block Public Access.
fn catalogue_with_free() -> String {
    let mut c: Value = serde_json::from_str(CATALOGUE).unwrap();
    c["free"] = json!([
        {"type": "aws:iam/role:Role", "reason": "IAM is offered at no additional charge."},
        {"type": "aws:s3/bucketPublicAccessBlock:BucketPublicAccessBlock",
         "reason": "S3 charges for storage, requests and transfer, not for a bucket's settings."}
    ]);
    c.to_string()
}

/// A change of an RDS instance, two new IAM roles, a new Block Public Access and a new queue, in a stack that also
/// holds an unchanged role and an unchanged security group.
fn free_preview(with_priced: bool) -> String {
    let step = |op: &str, kind: &str, name: &str, inputs: Value| {
        let urn = format!("urn:pulumi:prod::shop::{kind}::{name}");
        let state = json!({"urn": urn, "custom": true, "type": kind, "inputs": inputs});
        json!({"op": op, "urn": urn, "oldState": state, "newState": state})
    };
    let mut steps = vec![
        step("create", "aws:iam/role:Role", "api", json!({"name": "api-role"})),
        step("create", "aws:iam/role:Role", "worker", json!({})),
        step(
            "create",
            "aws:s3/bucketPublicAccessBlock:BucketPublicAccessBlock",
            "assets-block",
            json!({"blockPublicAcls": true}),
        ),
        step("same", "aws:iam/role:Role", "deploy", json!({})),
        step("same", "aws:ec2/securityGroup:SecurityGroup", "db", json!({})),
    ];
    if with_priced {
        steps.insert(
            0,
            step(
                "update",
                "aws:rds/instance:Instance",
                "orders",
                json!({"instanceClass": "db.r6i.xlarge"}),
            ),
        );
        steps.push(step("create", "aws:sqs/queue:Queue", "jobs", json!({})));
    }
    json!({"config": {"aws:region": "eu-west-1"}, "steps": steps}).to_string()
}

#[test]
fn a_free_type_is_neither_sent_nor_not_priced() {
    let e = estimate_with(&free_preview(true), &catalogue_with_free(), &Options::default());
    let types: Vec<&str> = e.request.resources.iter().map(|r| r.type_token.as_str()).collect();
    assert_eq!(
        types,
        ["aws:rds/instance:Instance"],
        "free types are not sent, changed or unchanged"
    );
    assert_eq!(
        serde_json::to_value(&e.request.not_priced).unwrap(),
        json!([{"type": "aws:sqs/queue:Queue", "count": 1}])
    );
    assert_eq!(e.stack_not_priced, 1, "the security group; the unchanged role is free");
    let free: Vec<(&str, u32)> = e.free.iter().map(|f| (f.label(), f.count)).collect();
    assert_eq!(
        free,
        [("Role", 2), ("BucketPublicAccessBlock", 1)],
        "changed ones, by type, in order"
    );
    assert_eq!(e.locals.len(), 1);
}

#[test]
fn a_catalogue_without_free_behaves_as_before() {
    let e = estimate(&free_preview(true), &Options::default());
    assert!(e.free.is_empty());
    assert_eq!(
        serde_json::to_value(&e.request.not_priced).unwrap(),
        json!([
            {"type": "aws:iam/role:Role", "count": 2},
            {"type": "aws:s3/bucketPublicAccessBlock:BucketPublicAccessBlock", "count": 1},
            {"type": "aws:sqs/queue:Queue", "count": 1}
        ])
    );
    assert_eq!(e.stack_not_priced, 2);
    let response: Response =
        serde_json::from_value(json!({"formatVersion": 1, "currency": "USD", "resources": []})).unwrap();
    let comment = render::comment(&response, &e);
    assert!(!comment.contains("No charge"), "{comment}");
}

#[test]
fn the_comment_says_no_charge_of_their_own() {
    let e = estimate_with(&free_preview(true), &catalogue_with_free(), &Options::default());
    let orders = key("urn:pulumi:prod::shop::aws:rds/instance:Instance::orders");
    let response: Response = serde_json::from_value(json!({"formatVersion": 1, "currency": "USD", "resources": [
        {"key": orders, "monthlyBefore": 398.0, "monthlyAfter": 398.0, "basis": "company-rate"}]}))
    .unwrap();

    let markdown = render::render(&response, &e, render::Style::Markdown);
    assert!(
        markdown.contains(
            "**Not priced**\n\n- 1 × `aws:sqs/queue:Queue`: not priced by qloudpulse yet\n\n**No charge of their \
             own**\n\n- 2 × `Role`: IAM is offered at no additional charge.\n- 1 × `BucketPublicAccessBlock`: S3 \
             charges for storage, requests and transfer, not for a bucket's settings.\n\n"
        ),
        "{markdown}"
    );
    assert!(
        markdown.contains(
            "**The stack:** about $398 a month now, about $398 after this change, from 1 priced resource; 2 not priced."
        ),
        "the queue and the security group; no free one: {markdown}"
    );
    assert!(!markdown.contains("iam/role"), "{markdown}");

    let text = render::render(&response, &e, render::Style::Text);
    assert!(
        text.contains("2 × Role, 1 × BucketPublicAccessBlock: no charge of their own.\n"),
        "{text}"
    );
}

#[test]
fn a_change_of_only_free_types_adds_no_cost() {
    let free = std::env::temp_dir().join(format!("qloudpulse-catalogue-free-{}.json", std::process::id()));
    std::fs::write(&free, catalogue_with_free()).unwrap();
    let args = [
        "estimate",
        "--endpoint",
        "https://127.0.0.1:9/v1/estimates",
        "--format",
        "markdown",
    ];

    let (ok, stdout, stderr) = run(
        &[&args[..], &["--catalogue", free.to_str().unwrap()]].concat(),
        &free_preview(false),
        Some("qpev_test"),
    );
    assert!(ok, "{stderr}");
    assert!(
        stdout.starts_with("### qloudpulse: this change adds no monthly cost\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("Not priced"), "{stdout}");
    assert!(stdout.contains("- 2 × `Role`: IAM is offered"), "{stdout}");

    // The shipped list has no `free`: as before, they are not priced.
    let (ok, stdout, stderr) = run(&args, &free_preview(false), Some("qpev_test"));
    assert!(ok, "{stderr}");
    assert!(
        stdout.starts_with("### qloudpulse: nothing in this change could be priced\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("- 2 × `aws:iam/role:Role`: not priced by qloudpulse yet"),
        "{stdout}"
    );
}
