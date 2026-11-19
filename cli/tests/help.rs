//! The help texts say how to give the client its token, short and long, at the top and under `estimate`.

use std::process::Command;

fn help(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_qloudpulse"))
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?} failed");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn every_help_names_the_token_and_where_it_is_made() {
    for args in [&["--help"][..], &["-h"], &["estimate", "--help"], &["estimate", "-h"]] {
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
fn without_a_token_it_says_where_to_get_one() {
    let out = Command::new(env!("CARGO_BIN_EXE_qloudpulse"))
        .args(["estimate", "tests/fixtures/preview-update.json"])
        .env_remove("QLOUDPULSE_TOKEN")
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
