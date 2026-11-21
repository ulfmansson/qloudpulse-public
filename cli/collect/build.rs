//! The version `qloudpulse-collect --version` prints. A release build names the commit it was built from, as rustc and cargo
//! do: `0.3.1 (1ab39062b 2026-11-21)`. cli/collect/release.sh passes the commit in, as the Docker builds see `cli/` and not the
//! repository's `.git`. A plain `cargo build` has no commit to name and prints the version alone.
//!
//! The commit and its date come from git, never from the clock, so building the same commit again gives the same
//! binary. The commit is a label for support; that a binary was built from it is shown by the release's signed
//! provenance (cli/collect/README.md, "Verify what you run").

use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=QLOUDPULSE_COMMIT");
    println!("cargo:rerun-if-env-changed=QLOUDPULSE_COMMIT_DATE");
    let version = env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let commit = env::var("QLOUDPULSE_COMMIT").unwrap_or_default();
    let date = env::var("QLOUDPULSE_COMMIT_DATE").unwrap_or_default();
    let long = match (commit.as_str(), date.as_str()) {
        ("", "") => version,
        (commit, date) => {
            // A source tree with changes is marked, so such a binary is never taken for a build of the commit alone.
            let (hash, modified) = match commit.strip_suffix("-modified") {
                Some(hash) => (hash, "-modified"),
                None => (commit, ""),
            };
            assert!(
                hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "QLOUDPULSE_COMMIT is {commit:?}, not a full commit hash (with -modified for a changed tree)"
            );
            assert!(
                date.len() == 10
                    && date.bytes().enumerate().all(|(i, b)| match i {
                        4 | 7 => b == b'-',
                        _ => b.is_ascii_digit(),
                    }),
                "QLOUDPULSE_COMMIT_DATE is {date:?}, not YYYY-MM-DD"
            );
            format!("{version} ({}{modified} {date})", &hash[..9])
        }
    };
    println!("cargo:rustc-env=QLOUDPULSE_LONG_VERSION={long}");
}
