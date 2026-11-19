//! The arguments of `qloudpulse pulumi preview` and `qloudpulse pulumi up`, which run Pulumi with the user's own
//! arguments and price the change in between.
//!
//! Pulumi prints its human diff or its JSON, never both, and its `--event-log` (a debug flag, only there with
//! `PULUMI_DEBUG_COMMANDS`) writes `apitype.StepEventStateMetadata`, which has no `propertyDependencies` and no stack
//! config. So the change is priced from a second run, `pulumi preview --json --show-sames`, whose arguments are built
//! here from the user's.
//!
//! Pulumi's flags are read as separate words only: `-y`, `--yes` and `--yes=true`, never a cluster such as `-yf`.

use std::ffi::{OsStr, OsString};

/// What `pulumi up` takes and `pulumi preview` refuses (Pulumi's `pkg/cmd/pulumi/operations/up.go` beside
/// `preview.go`), left out of the priced preview. `--expect-no-changes` sets only the exit code, which must not keep
/// the change from being priced, so it goes too.
const UP_ONLY_FLAGS: &[&str] = &[
    "-y",
    "--yes",
    "-f",
    "--skip-preview",
    "--continue-on-error",
    "--strict",
    "--expect-no-changes",
];

/// The flags of [`UP_ONLY_FLAGS`]'s kind that take a value, as `--plan plan.json` or `--plan=plan.json`.
const UP_ONLY_VALUE_FLAGS: &[&str] = &["--plan", "--secrets-provider"];

/// The flag's name, without an `=value`: `--yes=true` is `--yes`. A word that is not UTF-8 is a value, not a flag.
fn flag_name(word: &OsStr) -> Option<&str> {
    let word = word.to_str()?;
    if !word.starts_with('-') || word == "-" {
        return None;
    }
    Some(word.split_once('=').map_or(word, |(name, _)| name))
}

/// The words before a bare `--`, after which Pulumi reads none as its flag.
fn flags(args: &[OsString]) -> impl Iterator<Item = &OsStr> {
    args.iter().map(OsString::as_os_str).take_while(|w| *w != "--")
}

/// Whether the arguments leave a boolean flag on: `--yes`, `-y` or `--yes=true`, not `--yes=false`. The last one
/// wins, as in Pulumi's flag parser (pflag).
fn is_on(args: &[OsString], long: &str, short: Option<&str>) -> bool {
    let mut on = false;
    for word in flags(args).filter_map(OsStr::to_str) {
        if word == long || Some(word) == short {
            on = true;
        } else if let Some(value) = word.strip_prefix(long).and_then(|r| r.strip_prefix('=')) {
            // Go's strconv.ParseBool, which pflag reads a boolean's value with.
            on = matches!(value, "1" | "t" | "T" | "true" | "TRUE" | "True");
        }
    }
    on
}

/// Whether the user asked Pulumi for JSON: `--json`, `-j`, or `--json=true`.
pub fn wants_json(args: &[OsString]) -> bool {
    is_on(args, "--json", Some("-j"))
}

/// Whether `pulumi up` skips its preview (`--skip-preview`, `-f`): then nothing is priced.
pub fn skips_preview(args: &[OsString]) -> bool {
    is_on(args, "--skip-preview", Some("-f"))
}

/// Whether `pulumi up` goes ahead without asking (`--yes`, `-y`).
pub fn says_yes(args: &[OsString]) -> bool {
    is_on(args, "--yes", Some("-y"))
}

/// The arguments of the priced preview, from those the user gave `pulumi preview` or `pulumi up`: `preview`, theirs
/// without what `up` alone takes, then `--json`, and `--show-sames` unless they gave it, so the stack's unchanged
/// resources are listed for its cost. `--json` goes in front of a bare `--`, where Pulumi still reads it as a flag.
pub fn priced_preview(args: &[OsString]) -> Vec<OsString> {
    let mut out: Vec<OsString> = vec!["preview".into()];
    let end = args.iter().position(|w| w == "--").unwrap_or(args.len());
    let mut words = args[..end].iter();
    while let Some(word) = words.next() {
        match flag_name(word) {
            Some(name) if UP_ONLY_FLAGS.contains(&name) => {}
            Some(name) if UP_ONLY_VALUE_FLAGS.contains(&name) => {
                // `--plan=plan.json` carries its value; `--plan plan.json` takes the next word.
                if !word.to_str().is_some_and(|w| w.contains('=')) {
                    words.next();
                }
            }
            _ => out.push(word.clone()),
        }
    }
    out.push("--json".into());
    if !is_on(args, "--show-sames", None) {
        out.push("--show-sames".into());
    }
    out.extend(args[end..].iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<OsString> {
        s.split_whitespace().map(OsString::from).collect()
    }

    fn joined(v: &[OsString]) -> String {
        v.iter().map(|w| w.to_str().unwrap()).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn the_priced_preview_adds_json_and_sames() {
        assert_eq!(
            joined(&priced_preview(&words("-s prod --diff"))),
            "preview -s prod --diff --json --show-sames"
        );
        assert_eq!(joined(&priced_preview(&[])), "preview --json --show-sames");
    }

    #[test]
    fn sames_the_user_asked_for_are_not_doubled() {
        assert_eq!(
            joined(&priced_preview(&words("--show-sames -s prod"))),
            "preview --show-sames -s prod --json"
        );
        assert_eq!(
            joined(&priced_preview(&words("--show-sames=false"))),
            "preview --show-sames=false --json --show-sames",
            "the last one wins in Pulumi's flag parser"
        );
    }

    #[test]
    fn what_up_alone_takes_is_left_out_of_the_preview() {
        assert_eq!(
            joined(&priced_preview(&words(
                "-y -s prod --yes=true --continue-on-error --strict --plan plan.json --secrets-provider=awskms://k \
                 --expect-no-changes --target urn:a"
            ))),
            "preview -s prod --target urn:a --json --show-sames"
        );
        assert_eq!(
            joined(&priced_preview(&words("--plan=plan.json -m msg"))),
            "preview -m msg --json --show-sames"
        );
    }

    #[test]
    fn words_after_a_bare_double_dash_are_not_flags() {
        assert_eq!(
            joined(&priced_preview(&words("-s prod -- -y"))),
            "preview -s prod --json --show-sames -- -y"
        );
        assert!(!says_yes(&words("-- -y")));
        assert!(!wants_json(&words("-- --json")));
        assert!(!skips_preview(&words("-- -f")));
    }

    #[test]
    fn boolean_flags_are_read_with_their_values() {
        for on in ["--yes", "-y", "--yes=true", "--yes=1", "--yes=TRUE"] {
            assert!(says_yes(&words(&format!("-s prod {on}"))), "{on}");
        }
        for off in ["--yes=false", "--yes=0", "--yesterday", "-yf"] {
            assert!(!says_yes(&words(off)), "{off}");
        }
        for on in ["--skip-preview", "-f", "--skip-preview=true"] {
            assert!(skips_preview(&words(on)), "{on}");
        }
        assert!(!skips_preview(&words("--skip-preview=false -s prod")));
        assert!(!says_yes(&words("-y --yes=false")), "the last one wins");
        for on in ["--json", "-j", "--json=true"] {
            assert!(wants_json(&words(on)), "{on}");
        }
        assert!(!wants_json(&words("--json=false")));
    }
}
