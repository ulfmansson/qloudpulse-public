//! Newer releases of a qloudpulse client on GitHub, and `update`, which installs one over the running binary with the
//! release's own install script. That script checks the archive against the release's `SHA256SUMS` (and with
//! `--attest` its signed provenance); the script itself is checked against the same `SHA256SUMS` before it runs.
//!
//! Both clients use this module: `qloudpulse-stack` and `qloudpulse-collect`, each with its own [`Client`]. The newest
//! release is read from GitHub's list of the repository's releases, as `install.sh` does, and not from GitHub's
//! "latest", which is the cluster agent's release.
//!
//! [`daily_notice`] is the check `qloudpulse-collect` makes from the commands a person runs: GitHub at most once a day,
//! remembered in a small file, and nothing said when GitHub cannot be reached. `qloudpulse-stack` learns of a newer
//! release from qloudpulse's catalogue instead, on every run that prices (see [`crate::version`]).

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::version::compare;

/// Where the releases are.
pub const REPO: &str = "ulfmansson/qloudpulse-public";

/// Turns [`daily_notice`] off when set to anything but empty or `0`.
pub const NO_CHECK_ENV: &str = "QLOUDPULSE_NO_UPDATE_CHECK";

/// How long a check is remembered: at most one call to GitHub a day.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// How long [`daily_notice`] waits for GitHub. A machine whose egress drops the call waits this long once a day.
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
/// GitHub's list of releases runs to a few MB with every asset named; anything past this is not that list.
const MAX_BODY: u64 = 32 * 1024 * 1024;

/// A client, as its releases are tagged (`<name>-<version>`) and its binary is named.
#[derive(Debug, Clone, Copy)]
pub struct Client {
    pub name: &'static str,
    /// The running binary's version.
    pub version: &'static str,
    /// The client's README in the repository, for what to do when `update` cannot.
    pub readme: &'static str,
}

/// `qloudpulse-stack` itself.
pub const STACK: Client = Client {
    name: "qloudpulse-stack",
    version: env!("CARGO_PKG_VERSION"),
    readme: "cli/stack/README.md",
};

/// What `update` takes.
#[derive(Debug, Default, Clone)]
pub struct Options {
    /// Say whether a newer release is out, and install nothing.
    pub check: bool,
    /// Install this release, newer or older, instead of the newest.
    pub version: Option<String>,
    /// Also check the archive's signed build provenance, with `gh attestation verify`.
    pub attest: bool,
}

impl Client {
    /// `update`: the newest release, or the one asked for, installed over this binary.
    pub fn update(&self, options: &Options) -> Result<()> {
        let agent = self.agent(Duration::from_secs(60));
        let wanted = match &options.version {
            Some(v) if !options.check => self.normalize(v)?,
            _ => {
                let newest = self.newest(&agent)?;
                remember(self, Some(&newest));
                if !self.is_newer(&newest) {
                    eprintln!("{} {} is the newest release", self.name, self.version);
                    return Ok(());
                }
                if options.check {
                    eprintln!("{}", self.notice(&newest));
                    return Ok(());
                }
                newest
            }
        };
        let exe = self.this_binary()?;
        let dir = exe.parent().context("this binary has no directory")?;
        eprintln!(
            "{}: installing {wanted} over {} in {}",
            self.name,
            self.version,
            dir.display()
        );
        self.install(&agent, &wanted, &exe, options.attest)
    }

    /// What to say when `latest` is newer than this binary.
    pub fn notice(&self, latest: &str) -> String {
        format!(
            "{name} {latest} is out (this is {}): `{name} update` installs it",
            self.version,
            name = self.name
        )
    }

    /// Whether `version` is newer than this binary.
    pub fn is_newer(&self, version: &str) -> bool {
        compare(self.version, version) == Some(Ordering::Less)
    }

    /// A version as `update --version` takes it: `0.5.0`, `v0.5.0` or the tag `<name>-0.5.0`.
    pub fn normalize(&self, v: &str) -> Result<String> {
        let v = v.trim();
        let v = v.strip_prefix(&format!("{}-", self.name)).unwrap_or(v);
        let v = v.strip_prefix('v').unwrap_or(v);
        if compare(v, v).is_none() {
            bail!("not a version: {v} (for example 0.5.0)");
        }
        Ok(v.to_string())
    }

    /// The newest of this client's releases in GitHub's list, drafts and pre-releases left out.
    pub fn newest_in(&self, releases: &str) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Release {
            tag_name: String,
            #[serde(default)]
            draft: bool,
            #[serde(default)]
            prerelease: bool,
        }
        let releases: Vec<Release> =
            serde_json::from_str(releases).context("GitHub's list of releases is not in the form expected")?;
        let prefix = format!("{}-", self.name);
        Ok(releases
            .into_iter()
            .filter(|r| !r.draft && !r.prerelease)
            .filter_map(|r| r.tag_name.strip_prefix(&prefix).map(str::to_string))
            .filter(|v| compare(v, v).is_some())
            .max_by(|a, b| compare(a, b).unwrap_or(Ordering::Equal)))
    }

    /// The newest release, asked of GitHub.
    pub fn newest(&self, agent: &ureq::Agent) -> Result<String> {
        let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=100");
        let body = get(agent, &url, "application/vnd.github+json")?;
        let body = String::from_utf8(body).context("GitHub's list of releases is not text")?;
        self.newest_in(&body)?
            .with_context(|| format!("github.com/{REPO} has no {} release yet", self.name))
    }

    fn agent(&self, timeout: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .user_agent(format!("{}/{} (update)", self.name, self.version))
            .build()
            .into()
    }

    /// The binary running now, links followed, so the update lands where it really is. Refused when it is not named
    /// as the release names it: the install script would put the new one beside it, and this one would stay.
    fn this_binary(&self) -> Result<PathBuf> {
        let exe = std::env::current_exe().context("could not find this binary")?;
        let exe = exe.canonicalize().unwrap_or(exe);
        let expected = if cfg!(windows) {
            format!("{}.exe", self.name)
        } else {
            self.name.to_string()
        };
        if exe.file_name().and_then(|n| n.to_str()) != Some(expected.as_str()) {
            bail!(
                "this binary is {}, not {expected}, so update cannot replace it; install with the release's install \
                 script (https://github.com/{REPO}/blob/main/{})",
                exe.display(),
                self.readme
            );
        }
        Ok(exe)
    }

    /// Downloads the release's install script, checks it against the release's `SHA256SUMS`, and runs it for this
    /// binary's directory.
    fn install(&self, agent: &ureq::Agent, version: &str, exe: &Path, attest: bool) -> Result<()> {
        let dir = exe.parent().context("this binary has no directory")?;
        let script_name = if cfg!(windows) { "install.ps1" } else { "install.sh" };
        let base = format!("https://github.com/{REPO}/releases/download/{}-{version}", self.name);
        let sums = get(agent, &format!("{base}/SHA256SUMS"), "*/*")?;
        let sums = String::from_utf8(sums).context("the release's SHA256SUMS is not text")?;
        let want = sum_of(&sums, script_name).with_context(|| {
            format!(
                "{} {version}'s SHA256SUMS names no {script_name}; install it as {} says",
                self.name, self.readme
            )
        })?;
        let script = get(agent, &format!("{base}/{script_name}"), "*/*")?;
        let got = hex(&Sha256::digest(&script));
        if !got.eq_ignore_ascii_case(want) {
            bail!(
                "{script_name} does not match the release's SHA256SUMS (got {got}, want {want}); nothing was installed"
            );
        }

        let tmp = std::env::temp_dir().join(format!("{}-update-{}", self.name, std::process::id()));
        std::fs::create_dir_all(&tmp).with_context(|| format!("could not create {}", tmp.display()))?;
        let path = tmp.join(script_name);
        let ran = std::fs::write(&path, &script)
            .with_context(|| format!("could not write {}", path.display()))
            .and_then(|()| self.run_script(&path, version, dir, exe, attest));
        let _ = std::fs::remove_dir_all(&tmp);
        ran
    }

    #[cfg(not(windows))]
    fn run_script(&self, script: &Path, version: &str, dir: &Path, _exe: &Path, attest: bool) -> Result<()> {
        let mut sh = Command::new("sh");
        sh.arg(script).arg("--version").arg(version).arg("--dir").arg(dir);
        if attest {
            sh.arg("--attest");
        }
        let status = sh.status().context("could not run sh")?;
        if !status.success() {
            bail!(
                "the install script stopped ({status}); {} {} is still installed",
                self.name,
                self.version
            );
        }
        Ok(())
    }

    /// Windows will not overwrite a running program, but it will rename one: this binary steps aside as
    /// `<name>.exe.old` for the script, and comes back if the script fails. The next `update` removes what is left.
    #[cfg(windows)]
    fn run_script(&self, script: &Path, version: &str, dir: &Path, exe: &Path, attest: bool) -> Result<()> {
        let old = exe.with_extension("exe.old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, &old).with_context(|| format!("could not move {} aside", exe.display()))?;
        let mut ps = Command::new("powershell.exe");
        ps.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(script)
            .arg("-Version")
            .arg(version)
            .arg("-Dir")
            .arg(dir)
            .arg("-NoModifyPath");
        if attest {
            ps.arg("-Attest");
        }
        let status = ps.status();
        if matches!(&status, Ok(s) if s.success()) && exe.exists() {
            return Ok(());
        }
        if !exe.exists() {
            let _ = std::fs::rename(&old, exe);
        }
        match status {
            Ok(s) => bail!(
                "the install script stopped ({s}); {} {} is still installed",
                self.name,
                self.version
            ),
            Err(e) => Err(anyhow::Error::new(e).context("could not run powershell.exe")),
        }
    }
}

/// What a person running `client` should hear about a newer release: a line when one is out, else nothing. Asks
/// GitHub at most once a day and remembers the answer (or that it got none) in the user's cache directory, so a
/// machine that cannot reach GitHub waits [`CHECK_TIMEOUT`] once a day and says nothing. Nothing at all when
/// [`NO_CHECK_ENV`] is set.
pub fn daily_notice(client: &Client) -> Option<String> {
    if std::env::var(NO_CHECK_ENV).is_ok_and(|v| !v.is_empty() && v != "0") {
        return None;
    }
    notice_from(client, cache_file(client).as_deref(), now(), || {
        client.newest(&client.agent(CHECK_TIMEOUT)).ok()
    })
}

/// [`daily_notice`] with its cache file, clock and question to GitHub given.
fn notice_from(
    client: &Client,
    cache: Option<&Path>,
    now: u64,
    ask: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let cached: Option<Checked> = cache
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok());
    let latest = match cached {
        Some(c) if is_fresh(c.checked, now) => c.latest,
        stale => {
            // No answer keeps what an earlier check found, until GitHub answers again.
            let latest = ask().or(stale.and_then(|c| c.latest));
            if let Some(path) = cache {
                write_cache(path, now, latest.as_deref());
            }
            latest
        }
    };
    latest.filter(|l| client.is_newer(l)).map(|l| client.notice(&l))
}

/// What the last check found, kept in `<cache dir>/qloudpulse/<name>-update.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Checked {
    /// Seconds since 1970 when GitHub was last asked, answered or not.
    checked: u64,
    /// The newest release then known, if any.
    latest: Option<String>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A check is fresh for a day; one from the future (a clock set back) is not.
fn is_fresh(checked: u64, now: u64) -> bool {
    checked <= now && now - checked < CHECK_EVERY.as_secs()
}

/// Notes what a check found. A cache that cannot be written only means GitHub is asked again next time.
fn remember(client: &Client, latest: Option<&str>) {
    if let Some(path) = cache_file(client) {
        write_cache(&path, now(), latest);
    }
}

fn write_cache(path: &Path, now: u64, latest: Option<&str>) {
    let checked = Checked {
        checked: now,
        latest: latest.map(str::to_string),
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(&checked) {
        let _ = std::fs::write(path, json);
    }
}

/// `%LOCALAPPDATA%\qloudpulse` on Windows, else `$XDG_CACHE_HOME/qloudpulse` or `~/.cache/qloudpulse`.
fn cache_file(client: &Client) -> Option<PathBuf> {
    let absolute = |v: std::ffi::OsString| Some(PathBuf::from(v)).filter(|p| p.is_absolute());
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").and_then(absolute)?
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .and_then(absolute)
            .or_else(|| std::env::var_os("HOME").and_then(absolute).map(|h| h.join(".cache")))?
    };
    Some(base.join("qloudpulse").join(format!("{}-update.json", client.name)))
}

fn get(agent: &ureq::Agent, url: &str, accept: &str) -> Result<Vec<u8>> {
    let mut answer = agent
        .get(url)
        .header("Accept", accept)
        .call()
        .with_context(|| format!("could not reach {url}"))?;
    let status = answer.status();
    if !status.is_success() {
        bail!("{url} answered {status}");
    }
    answer
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_vec()
        .with_context(|| format!("could not read {url}"))
}

/// The sum `SHA256SUMS` gives a file, as `sha256sum` writes it (`<hex>  <name>`, or `*<name>` in binary mode).
pub fn sum_of<'a>(sums: &'a str, file: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (hex, name) = line.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start();
        (name == file || name.strip_prefix('*') == Some(file)).then_some(hex)
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT_0_4: Client = Client {
        name: "qloudpulse-stack",
        version: "0.4.0",
        readme: "cli/stack/README.md",
    };

    #[test]
    fn the_newest_is_this_clients_by_number_without_drafts_or_prereleases() {
        let list = r#"[
            {"tag_name": "qloudpulse-agent-0.9.0"},
            {"tag_name": "qloudpulse-collect-3.0.0"},
            {"tag_name": "qloudpulse-stack-0.9.0"},
            {"tag_name": "qloudpulse-stack-0.10.0"},
            {"tag_name": "qloudpulse-stack-0.11.0", "draft": true},
            {"tag_name": "qloudpulse-stack-0.12.0", "prerelease": true},
            {"tag_name": "qloudpulse-stack-1.0"},
            {"tag_name": "qloudpulse-cli-5.0.0"}
        ]"#;
        assert_eq!(AT_0_4.newest_in(list).unwrap().as_deref(), Some("0.10.0"));
        assert_eq!(
            AT_0_4.newest_in(r#"[{"tag_name": "qloudpulse-agent-0.1.0"}]"#).unwrap(),
            None
        );
        assert!(AT_0_4.newest_in("{}").is_err());
    }

    #[test]
    fn newer_is_by_number() {
        assert!(AT_0_4.is_newer("0.10.0"));
        assert!(AT_0_4.is_newer("1.0.0"));
        assert!(!AT_0_4.is_newer("0.4.0"));
        assert!(!AT_0_4.is_newer("0.3.9"));
        assert!(!AT_0_4.is_newer("garbage"));
    }

    #[test]
    fn a_version_is_taken_bare_with_v_or_as_the_tag() {
        assert_eq!(AT_0_4.normalize("0.5.0").unwrap(), "0.5.0");
        assert_eq!(AT_0_4.normalize(" v0.5.0 ").unwrap(), "0.5.0");
        assert_eq!(AT_0_4.normalize("qloudpulse-stack-0.5.0").unwrap(), "0.5.0");
        assert!(AT_0_4.normalize("0.5").is_err());
        assert!(AT_0_4.normalize("latest").is_err());
    }

    #[test]
    fn the_notice_names_both_versions_and_the_command() {
        assert_eq!(
            AT_0_4.notice("0.5.0"),
            "qloudpulse-stack 0.5.0 is out (this is 0.4.0): `qloudpulse-stack update` installs it"
        );
    }

    #[test]
    fn sums_are_found_by_the_whole_name() {
        let sums =
            "aa11  install.sh\nbb22 *install.ps1\ncc33  qloudpulse-stack-0.5.0-x86_64-unknown-linux-musl/install.sh\n";
        assert_eq!(sum_of(sums, "install.sh"), Some("aa11"));
        assert_eq!(sum_of(sums, "install.ps1"), Some("bb22"));
        assert_eq!(sum_of(sums, "stall.sh"), None);
    }

    #[test]
    fn a_check_is_fresh_for_a_day_and_never_from_the_future() {
        let day = CHECK_EVERY.as_secs();
        assert!(is_fresh(1_000, 1_000));
        assert!(is_fresh(1_000, 1_000 + day - 1));
        assert!(!is_fresh(1_000, 1_000 + day));
        assert!(!is_fresh(2_000, 1_000));
    }

    #[test]
    fn github_is_asked_once_a_day_and_silence_keeps_the_last_answer() {
        let dir = std::env::temp_dir().join(format!("qloudpulse-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = dir.join("qloudpulse").join("qloudpulse-stack-update.json");
        let day = CHECK_EVERY.as_secs();
        let asked = std::cell::Cell::new(0);
        let answer = |v: Option<&'static str>| {
            asked.set(asked.get() + 1);
            v.map(str::to_string)
        };

        // No cache: asked, and the answer is said and kept.
        let said = notice_from(&AT_0_4, Some(&cache), 1_000, || answer(Some("0.5.0")));
        assert_eq!(said, Some(AT_0_4.notice("0.5.0")));
        assert_eq!(asked.get(), 1);
        // Within the day: the kept answer, nothing asked.
        let said = notice_from(&AT_0_4, Some(&cache), 1_000 + day - 1, || answer(Some("9.9.9")));
        assert_eq!(said, Some(AT_0_4.notice("0.5.0")));
        assert_eq!(asked.get(), 1);
        // A day on, GitHub does not answer: the last answer stands, and the attempt counts as the day's check.
        let said = notice_from(&AT_0_4, Some(&cache), 1_000 + day, || answer(None));
        assert_eq!(said, Some(AT_0_4.notice("0.5.0")));
        assert_eq!(asked.get(), 2);
        let _ = notice_from(&AT_0_4, Some(&cache), 1_000 + day + 1, || answer(None));
        assert_eq!(asked.get(), 2);
        // The newest is this one: nothing said.
        let said = notice_from(&AT_0_4, Some(&cache), 1_000 + 3 * day, || answer(Some("0.4.0")));
        assert_eq!(said, None);
        // No cache directory at all: asked every time, said all the same.
        let said = notice_from(&AT_0_4, None, 0, || answer(Some("0.5.0")));
        assert_eq!(said, Some(AT_0_4.notice("0.5.0")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
    }
}
