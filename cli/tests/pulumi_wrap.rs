//! `qloudpulse pulumi preview` and `qloudpulse pulumi up` against a fake `pulumi` on PATH: a shell script that logs
//! each run's arguments, prints a fixture preview for `--json`, and a line of its own for the human display and `up`.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

const UPDATE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/preview-update.json");
const CATALOGUE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/catalogue.json");

const FAKE_PULUMI: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_PULUMI_LOG"
for word in "$@"; do
  if [ "$word" = "--json" ]; then
    cat "$FAKE_PULUMI_JSON"
    exit 0
  fi
done
case "$1" in
  preview) echo "fake pulumi: the diff"; exit "${FAKE_PREVIEW_EXIT:-0}" ;;
  up) echo "fake pulumi: updating"; exit "${FAKE_UP_EXIT:-0}" ;;
esac
exit 99
"#;

/// A directory of its own for each test, with the fake `pulumi` in `bin/`.
struct Fake {
    dir: PathBuf,
}

impl Fake {
    fn new(test: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("qloudpulse-wrap-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let pulumi = dir.join("bin/pulumi");
        std::fs::write(&pulumi, FAKE_PULUMI).unwrap();
        std::fs::set_permissions(&pulumi, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fake { dir }
    }

    fn preview_json(&self, json: &str) -> PathBuf {
        let path = self.dir.join("preview.json");
        std::fs::write(&path, json).unwrap();
        path
    }

    /// Each run of the fake `pulumi`, its arguments as one line.
    fn runs(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn command(&self, args: &[&str], preview: &Path) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_qloudpulse"));
        cmd.args(args)
            .env("PATH", format!("{}:/usr/bin:/bin", self.dir.join("bin").display()))
            .env("FAKE_PULUMI_LOG", self.dir.join("log"))
            .env("FAKE_PULUMI_JSON", preview)
            .env_remove("QLOUDPULSE_TOKEN")
            .env_remove("QLOUDPULSE_ENDPOINT")
            .env("QLOUDPULSE_CATALOGUE", CATALOGUE_PATH)
            .stdin(Stdio::null());
        cmd
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(mut cmd: Command) -> Run {
    let out = cmd.output().unwrap();
    Run {
        code: out.status.code(),
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
    }
}

/// The JSON that follows the fake's own line on stdout.
fn json_after(stdout: &str, line: &str) -> Value {
    let (_, after) = stdout
        .split_once(line)
        .unwrap_or_else(|| panic!("no {line:?} in {stdout}"));
    serde_json::from_str(after).unwrap_or_else(|e| panic!("{e}: {after}"))
}

#[test]
fn preview_shows_pulumis_diff_then_prices_the_change() {
    let fake = Fake::new("preview-dry-run");
    let r = run(fake.command(
        &["pulumi", "preview", "--dry-run", "-s", "dev", "--diff"],
        Path::new(UPDATE),
    ));
    assert_eq!(r.code, Some(0), "{}", r.stderr);
    assert_eq!(
        fake.runs(),
        ["preview -s dev --diff", "preview -s dev --diff --json --show-sames"],
        "the user's arguments as they are, then the priced preview"
    );
    let request = json_after(&r.stdout, "fake pulumi: the diff\n");
    assert!(!request["resources"].as_array().unwrap().is_empty(), "{request}");
}

#[test]
fn preview_prints_the_comment_after_the_diff() {
    let fake = Fake::new("preview-comment");
    let empty = fake.preview_json(r#"{"steps": []}"#);
    // Nothing to price makes no call; the endpoint is a closed port, so a call would fail the run.
    let mut cmd = fake.command(
        &[
            "pulumi",
            "preview",
            "--endpoint",
            "https://127.0.0.1:9/v1/estimates",
            "-s",
            "dev",
        ],
        &empty,
    );
    cmd.env("QLOUDPULSE_TOKEN", "qpev_test");
    let r = run(cmd);
    assert_eq!(r.code, Some(0), "{}", r.stderr);
    let diff = r.stdout.find("fake pulumi: the diff").expect(&r.stdout);
    let comment = r
        .stdout
        .find("### qloudpulse: nothing in this change could be priced")
        .expect(&r.stdout);
    assert!(diff < comment, "{}", r.stdout);
}

#[test]
fn a_failed_preview_ends_with_pulumis_code_and_prices_nothing() {
    let fake = Fake::new("preview-fails");
    let mut cmd = fake.command(&["pulumi", "preview", "--dry-run", "-s", "dev"], Path::new(UPDATE));
    cmd.env("FAKE_PREVIEW_EXIT", "3");
    let r = run(cmd);
    assert_eq!(r.code, Some(3));
    assert_eq!(fake.runs(), ["preview -s dev"]);
}

#[test]
fn a_failed_estimate_of_a_preview_exits_1_after_the_diff() {
    let fake = Fake::new("preview-no-token");
    let r = run(fake.command(&["pulumi", "preview", "-s", "dev"], Path::new(UPDATE)));
    assert_eq!(r.code, Some(1));
    assert!(r.stdout.contains("fake pulumi: the diff"), "{}", r.stdout);
    assert!(r.stderr.contains("QLOUDPULSE_TOKEN"), "{}", r.stderr);
    assert_eq!(
        fake.runs(),
        ["preview -s dev"],
        "without a token the stack is not previewed again for nothing"
    );
}

#[test]
fn up_prices_the_change_then_runs_up_with_the_users_arguments() {
    let fake = Fake::new("up-dry-run");
    let mut cmd = fake.command(
        &[
            "pulumi",
            "up",
            "--dry-run",
            "-s",
            "dev",
            "--yes",
            "--plan",
            "plan.json",
            "--target",
            "urn:a",
        ],
        Path::new(UPDATE),
    );
    cmd.env("FAKE_UP_EXIT", "7");
    let r = run(cmd);
    assert_eq!(r.code, Some(7), "Pulumi's exit code: {}", r.stderr);
    assert_eq!(
        fake.runs(),
        [
            "preview -s dev --target urn:a --json --show-sames",
            "up -s dev --yes --plan plan.json --target urn:a"
        ],
        "what only up takes is left out of the preview, and up gets everything"
    );
    let (request, up) = r.stdout.split_once("fake pulumi: updating").expect(&r.stdout);
    let request: Value = serde_json::from_str(request).unwrap();
    assert!(request["resources"].is_array());
    assert!(up.trim().is_empty());
    assert!(
        r.stderr.contains("--yes"),
        "says up goes ahead without asking: {}",
        r.stderr
    );
}

#[test]
fn up_with_skip_preview_prices_nothing() {
    for skip in ["--skip-preview", "-f"] {
        let fake = Fake::new(&format!("up-skip{skip}"));
        let mut cmd = fake.command(&["pulumi", "up", "--", "-s", "dev", skip], Path::new(UPDATE));
        cmd.env("QLOUDPULSE_TOKEN", "qpev_test");
        let r = run(cmd);
        assert_eq!(r.code, Some(0), "{}", r.stderr);
        assert_eq!(fake.runs(), [format!("up -s dev {skip}")]);
        assert!(r.stderr.contains("nothing is priced"), "{}", r.stderr);
    }

    let fake = Fake::new("up-skip-required");
    let r = run(fake.command(
        &["pulumi", "up", "--dry-run", "--require-estimate", "-f"],
        Path::new(UPDATE),
    ));
    assert_eq!(r.code, Some(1));
    assert!(fake.runs().is_empty(), "the two contradict each other: nothing runs");
}

#[test]
fn a_failed_estimate_does_not_stop_up() {
    let fake = Fake::new("up-no-token");
    let r = run(fake.command(&["pulumi", "up", "-s", "dev"], Path::new(UPDATE)));
    assert_eq!(r.code, Some(0), "{}", r.stderr);
    assert_eq!(
        fake.runs(),
        ["up -s dev"],
        "no token: no priced preview, up all the same"
    );
    let warning: Vec<&str> = r.stderr.lines().collect();
    assert_eq!(warning.len(), 1, "one line: {}", r.stderr);
    assert!(
        warning[0].starts_with("qloudpulse: no estimate, pulumi up goes on: set QLOUDPULSE_TOKEN"),
        "{}",
        r.stderr
    );
    assert_eq!(r.stdout, "fake pulumi: updating\n");
}

#[test]
fn require_estimate_stops_before_up() {
    let fake = Fake::new("up-required");
    let r = run(fake.command(&["pulumi", "up", "--require-estimate", "-s", "dev"], Path::new(UPDATE)));
    assert_eq!(r.code, Some(1));
    assert!(r.stderr.contains("--require-estimate"), "{}", r.stderr);
    assert!(fake.runs().is_empty(), "{:?}", fake.runs());
}

#[test]
fn the_token_is_never_printed() {
    let fake = Fake::new("up-network");
    // A priced change and a closed port: the send fails, and up goes on.
    let mut cmd = fake.command(
        &[
            "pulumi",
            "up",
            "--endpoint",
            "https://127.0.0.1:9/v1/estimates",
            "-s",
            "dev",
        ],
        Path::new(UPDATE),
    );
    cmd.env("QLOUDPULSE_TOKEN", "qpev_secret_token_never_shown");
    let r = run(cmd);
    assert_eq!(r.code, Some(0), "{}", r.stderr);
    assert_eq!(fake.runs(), ["preview -s dev --json --show-sames", "up -s dev"]);
    assert!(r.stderr.contains("no estimate, pulumi up goes on"), "{}", r.stderr);
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains("qpev_secret_token_never_shown"), "{out}");
    }
}

#[test]
fn pulumis_json_is_refused_before_anything_runs() {
    for (command, json) in [("preview", "--json"), ("up", "-j"), ("up", "--json=true")] {
        let fake = Fake::new(&format!("json-{command}{json}"));
        let r = run(fake.command(&["pulumi", command, "--dry-run", "-s", "dev", json], Path::new(UPDATE)));
        assert_eq!(r.code, Some(1), "{command} {json}");
        assert!(r.stderr.contains("qloudpulse estimate"), "{}", r.stderr);
        assert!(fake.runs().is_empty());
    }
}

#[test]
fn without_pulumi_on_path_it_says_so() {
    let fake = Fake::new("no-pulumi");
    let mut cmd = fake.command(&["pulumi", "up", "-s", "dev"], Path::new(UPDATE));
    cmd.env("PATH", "/usr/bin:/bin");
    let r = run(cmd);
    assert_eq!(r.code, Some(1));
    assert!(r.stderr.contains("pulumi is not on PATH"), "{}", r.stderr);
}
