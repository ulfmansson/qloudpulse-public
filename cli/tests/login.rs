//! `qloudpulse login` and `logout`, and the token they keep, through the binary. The keychain is never touched:
//! `QLOUDPULSE_CREDENTIALS_FILE` names a file in a directory of the test's own, as on a machine without a keychain.

use std::path::PathBuf;
use std::process::{Command, Stdio};

const TOKEN: &str = "qpev_acme.0a1b2c3d.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
const CATALOGUE_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/catalogue.json");

/// A directory of the test's own, removed when dropped.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let dir = std::env::temp_dir().join(format!("qloudpulse-login-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Dir(dir)
    }

    fn credentials(&self) -> PathBuf {
        self.0.join("credentials")
    }

    /// Writes the file as `qloudpulse login` does, with one token for this qloudpulse.
    fn keep(&self, origin: &str, expires_at: &str) {
        let text = format!(r#"{{"{origin}": {{"token": "{TOKEN}", "company": "acme", "expiresAt": "{expires_at}"}}}}"#);
        std::fs::write(self.credentials(), text).unwrap();
    }

    fn run(&self, args: &[&str], stdin: &str) -> (bool, String, String) {
        use std::io::Write as _;
        let mut child = Command::new(env!("CARGO_BIN_EXE_qloudpulse"))
            .args(args)
            .env_remove("QLOUDPULSE_TOKEN")
            .env_remove("QLOUDPULSE_ENDPOINT")
            .env("QLOUDPULSE_CATALOGUE", CATALOGUE_PATH)
            .env("QLOUDPULSE_CREDENTIALS_FILE", self.credentials())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        let (stdout, stderr) = (
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        );
        assert!(
            !stdout.contains(TOKEN) && !stderr.contains(TOKEN),
            "the token is never printed"
        );
        (out.status.success(), stdout, stderr)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_run_uses_the_kept_token_of_its_qloudpulse() {
    let dir = Dir::new("kept");
    dir.keep("https://app.qloudpulse.com", "2999-01-01T00:00:00Z");
    // Nothing to price makes no call, so the run passes once it has a token.
    let (ok, stdout, stderr) = dir.run(&["estimate", "--format", "markdown"], r#"{"steps": []}"#);
    assert!(ok, "{stderr}");
    assert!(stdout.starts_with("### qloudpulse: nothing in this change"), "{stdout}");

    let (ok, _, stderr) = dir.run(
        &["estimate", "--endpoint", "https://qloudpulse.example.com/v1/estimates"],
        r#"{"steps": []}"#,
    );
    assert!(
        !ok && stderr.contains("qloudpulse login"),
        "another qloudpulse's token is not this one's: {stderr}"
    );
}

#[test]
fn an_expired_kept_token_counts_as_none() {
    let dir = Dir::new("expired");
    dir.keep("https://app.qloudpulse.com", "2020-01-01T00:00:00Z");
    let (ok, _, stderr) = dir.run(&["estimate"], r#"{"steps": []}"#);
    assert!(!ok);
    assert!(
        stderr.contains("set QLOUDPULSE_TOKEN") && stderr.contains("qloudpulse login"),
        "{stderr}"
    );
}

#[test]
fn logout_removes_the_kept_token_and_says_it_still_exists_in_qloudpulse() {
    let dir = Dir::new("logout");
    dir.keep("https://app.qloudpulse.com", "2999-01-01T00:00:00Z");
    let (ok, _, stderr) = dir.run(&["logout"], "");
    assert!(ok, "{stderr}");
    assert!(stderr.contains("signed out of https://app.qloudpulse.com"), "{stderr}");
    assert!(
        stderr.contains("https://app.qloudpulse.com/ui/?profile#my-tokens"),
        "{stderr}"
    );
    assert!(!dir.credentials().exists());

    let (ok, _, stderr) = dir.run(&["logout"], "");
    assert!(ok, "{stderr}");
    assert!(stderr.contains("no token was kept"), "{stderr}");
}

#[test]
fn login_is_never_sent_in_the_clear_and_says_when_it_cannot_reach_qloudpulse() {
    let dir = Dir::new("login");
    let (ok, _, stderr) = dir.run(
        &["login", "--endpoint", "http://qloudpulse.example.com/v1/estimates"],
        "",
    );
    assert!(!ok && stderr.contains("must start with https://"), "{stderr}");

    // A closed port.
    let (ok, _, stderr) = dir.run(&["login", "--endpoint", "http://127.0.0.1:9/v1/estimates"], "");
    assert!(!ok);
    assert!(
        stderr.contains("could not reach qloudpulse (http://127.0.0.1:9/api/report/public/cli-login)"),
        "{stderr}"
    );
    assert!(!dir.credentials().exists());
}
