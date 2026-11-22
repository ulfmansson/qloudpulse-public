//! The client's own version against the ones qloudpulse names in its catalogue: a newer release is said, and one
//! below the oldest qloudpulse takes stops the run before anything is sent.

use std::cmp::Ordering;

use anyhow::{Result, bail};

use crate::catalogue::ClientVersions;

/// This client's version, from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What `qloudpulse-stack --version` prints after the name: the version, and in a release build the commit it was built
/// from and that commit's date, as `0.3.1 (1ab39062b 2026-11-21)` (see build.rs).
pub const LONG_VERSION: &str = env!("QLOUDPULSE_LONG_VERSION");

/// `x.y.z` as numbers, so `0.10.0` is newer than `0.9.0`.
fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// How `a` compares with `b`; nothing when either is not `x.y.z`.
pub fn compare(a: &str, b: &str) -> Option<Ordering> {
    Some(parse(a)?.cmp(&parse(b)?))
}

/// What to say about this client: an error below the minimum, a line when a newer release is out, else nothing.
pub fn check(mine: &str, client: Option<&ClientVersions>) -> Result<Option<String>> {
    let Some(c) = client else { return Ok(None) };
    if compare(mine, &c.minimum) == Some(Ordering::Less) {
        bail!(
            "this client is {mine}, and qloudpulse takes {} or later. Run `qloudpulse-stack update`, or download {} \
             from {}",
            c.minimum,
            c.latest,
            c.download
        );
    }
    if compare(mine, &c.latest) == Some(Ordering::Less) {
        return Ok(Some(format!(
            "qloudpulse-stack {} is out (this is {mine}): `qloudpulse-stack update` installs it, or download it from {}",
            c.latest, c.download
        )));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(latest: &str, minimum: &str) -> ClientVersions {
        ClientVersions {
            latest: latest.into(),
            minimum: minimum.into(),
            download: "https://example.com/get".into(),
        }
    }

    #[test]
    fn versions_compare_by_number() {
        assert_eq!(compare("0.10.0", "0.9.9"), Some(Ordering::Greater));
        assert_eq!(compare("1.2.3", "1.2.3"), Some(Ordering::Equal));
        assert_eq!(compare("0.1", "0.1.0"), None);
        assert_eq!(compare("0.1.0-beta", "0.1.0"), None);
    }

    #[test]
    fn below_the_minimum_stops_newer_is_said_current_is_quiet() {
        let err = check("0.1.0", Some(&versions("0.3.0", "0.2.0")))
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "this client is 0.1.0, and qloudpulse takes 0.2.0 or later. Run `qloudpulse-stack update`, or download 0.3.0 \
             from https://example.com/get"
        );
        assert_eq!(
            check("0.2.5", Some(&versions("0.3.0", "0.2.0"))).unwrap().as_deref(),
            Some(
                "qloudpulse-stack 0.3.0 is out (this is 0.2.5): `qloudpulse-stack update` installs it, or download it \
                 from https://example.com/get"
            )
        );
        assert_eq!(check("0.3.0", Some(&versions("0.3.0", "0.2.0"))).unwrap(), None);
        assert_eq!(check("0.4.0", Some(&versions("0.3.0", "0.2.0"))).unwrap(), None);
        assert_eq!(check("0.1.0", None).unwrap(), None);
    }
}
