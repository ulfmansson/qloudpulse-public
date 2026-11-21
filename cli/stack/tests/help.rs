//! The help texts say how to give the client its token, short and long, at the top and under every command, and the
//! Pulumi commands say how they run Pulumi.

use std::process::Command;

fn help(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_qloudpulse-stack"))
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?} failed");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn every_help_names_the_token_and_where_it_is_made() {
    for args in [
        &["--help"][..],
        &["-h"],
        &["estimate", "--help"],
        &["estimate", "-h"],
        &["pulumi", "--help"],
        &["pulumi", "preview", "--help"],
        &["pulumi", "up", "-h"],
        &["login", "--help"],
        &["logout", "-h"],
    ] {
        let text = help(args);
        assert!(
            text.contains("QLOUDPULSE_TOKEN"),
            "{args:?} does not name QLOUDPULSE_TOKEN:\n{text}"
        );
        assert!(
            text.contains("Your tokens") && text.contains("Settings, Events"),
            "{args:?} does not say where a token of your own, or an event source's, is made:\n{text}"
        );
        assert!(
            text.contains("https://app.qloudpulse.com/ui/?profile#my-tokens"),
            "{args:?} does not link hosted qloudpulse's Your tokens page:\n{text}"
        );
    }
    assert!(help(&["estimate", "-h"]).contains("QLOUDPULSE_ENDPOINT"));
}

#[test]
fn the_helps_name_login_and_where_it_keeps_the_token() {
    let top = help(&["--help"]);
    for said in ["login", "logout", "qloudpulse-stack login", "system keychain"] {
        assert!(top.contains(said), "--help does not say {said:?}:\n{top}");
    }
    let login = help(&["login", "--help"]);
    for said in [
        "one-time code",
        "Keychain",
        "Credential Manager",
        "Secret Service",
        "QLOUDPULSE_CREDENTIALS_FILE",
    ] {
        assert!(login.contains(said), "login --help does not say {said:?}:\n{login}");
    }
    assert!(help(&["logout", "--help"]).contains("still works until it expires or you remove it"));
    assert!(help(&["estimate", "--help"]).contains("QLOUDPULSE_CREDENTIALS_FILE"));
}

#[test]
fn the_pulumi_helps_say_how_they_run() {
    let preview = help(&["pulumi", "preview", "--help"]);
    assert!(preview.contains("previewed twice"), "{preview}");
    let up = help(&["pulumi", "up", "--help"]);
    for said in [
        "previewed twice",
        "-y/--yes",
        "without a pause",
        "--require-estimate",
        "-f/--skip-preview",
    ] {
        assert!(up.contains(said), "pulumi up --help does not say {said:?}:\n{up}");
    }
    for text in [&preview, &up] {
        assert!(text.contains("qloudpulse-stack's options come first"), "{text}");
    }
}

#[test]
fn without_a_token_it_says_where_to_get_one() {
    let out = Command::new(env!("CARGO_BIN_EXE_qloudpulse-stack"))
        .args(["estimate", "tests/fixtures/preview-update.json"])
        .env_remove("QLOUDPULSE_TOKEN")
        .env("QLOUDPULSE_CREDENTIALS_FILE", "/nonexistent/qloudpulse-credentials")
        // The shipped list, so the test reaches no network even if the token were asked for late.
        .env(
            "QLOUDPULSE_CATALOGUE",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/catalogue.json"),
        )
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(
        err.contains("QLOUDPULSE_TOKEN") && err.contains("https://app.qloudpulse.com/ui/?profile#my-tokens"),
        "{err}"
    );
}
