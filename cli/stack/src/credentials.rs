//! Where `qloudpulse-stack login` keeps its token: the system's keychain (the macOS Keychain, the Windows Credential Manager,
//! or Linux's Secret Service), one entry per qloudpulse, named by its address. Where there is no keychain, as on a
//! headless Linux machine, a file only its owner can read (0600): `credentials` in the config directory
//! (`$XDG_CONFIG_HOME/qloudpulse` or `~/.config/qloudpulse`, `%APPDATA%\qloudpulse` on Windows), or the file
//! `QLOUDPULSE_CREDENTIALS_FILE` names, which also leaves the keychain alone. A kept token past its `expiresAt` counts
//! as none. `QLOUDPULSE_TOKEN` wins over all of it (see [`crate::token`]).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::token::looks_like_token;

/// The keychain's service name for every entry the client keeps; the account is the qloudpulse's address.
pub const SERVICE: &str = "qloudpulse";

/// The variable that names a file to keep the token in instead of the keychain.
pub const FILE_VARIABLE: &str = "QLOUDPULSE_CREDENTIALS_FILE";

/// A token the client keeps, as qloudpulse answered it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Saved {
    pub token: String,
    /// The company the token is for, to say where the client is signed in.
    #[serde(default)]
    pub company: String,
    /// When the token stops working, as RFC 3339, such as `2027-02-17T08:01:00Z`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

impl Saved {
    /// Whether the token is past its `expiresAt` at `now` (seconds since 1970). One without a date, or with a date
    /// the client cannot read, is left to qloudpulse to refuse.
    pub fn expired(&self, now: i64) -> bool {
        self.expires_at
            .as_deref()
            .and_then(unix_seconds)
            .is_some_and(|at| now >= at)
    }
}

/// Where a token was kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kept {
    Keychain,
    File(PathBuf),
}

/// A system keychain. An `Err` means it cannot be used here (none, locked, refused), and the file is used instead.
pub trait Keychain {
    fn get(&self, account: &str) -> Result<Option<String>, String>;
    fn set(&self, account: &str, secret: &str) -> Result<(), String>;
    /// Whether there was an entry to delete.
    fn delete(&self, account: &str) -> Result<bool, String>;
}

/// The keychain of the system the client runs on, through the `keyring` crate.
pub struct SystemKeychain;

impl SystemKeychain {
    fn entry(account: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(SERVICE, account).map_err(|e| e.to_string())
    }
}

impl Keychain for SystemKeychain {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        match Self::entry(account)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        Self::entry(account)?.set_password(secret).map_err(|e| e.to_string())
    }

    fn delete(&self, account: &str) -> Result<bool, String> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// No keychain: everything goes to the file. Used when [`FILE_VARIABLE`] is set.
pub struct NoKeychain;

impl Keychain for NoKeychain {
    fn get(&self, _: &str) -> Result<Option<String>, String> {
        Err(format!("{FILE_VARIABLE} is set"))
    }
    fn set(&self, _: &str, _: &str) -> Result<(), String> {
        Err(format!("{FILE_VARIABLE} is set"))
    }
    fn delete(&self, _: &str) -> Result<bool, String> {
        Err(format!("{FILE_VARIABLE} is set"))
    }
}

/// The tokens the client keeps: in the keychain, else in the file.
pub struct Credentials<K> {
    keychain: K,
    file: Option<PathBuf>,
}

/// The file's contents: the kept tokens by the address of their qloudpulse.
type FileContents = BTreeMap<String, Saved>;

impl<K: Keychain> Credentials<K> {
    pub fn new(keychain: K, file: Option<PathBuf>) -> Self {
        Credentials { keychain, file }
    }

    /// The token kept for this qloudpulse, unless it has expired at `now` or is not shaped like a token.
    pub fn load(&self, account: &str, now: i64) -> Option<Saved> {
        let from_keychain = match self.keychain.get(account) {
            Ok(Some(secret)) => serde_json::from_str::<Saved>(&secret).ok(),
            Ok(None) | Err(_) => None,
        };
        from_keychain
            .or_else(|| self.read_file().ok()?.remove(account))
            .filter(|saved| looks_like_token(&saved.token) && !saved.expired(now))
    }

    /// Keeps the token for this qloudpulse: in the keychain if it takes it, else in the file. A token kept in the file
    /// before is removed from it once the keychain holds the new one.
    pub fn save(&self, account: &str, saved: &Saved) -> Result<Kept> {
        let secret = serde_json::to_string(saved)?;
        match self.keychain.set(account, &secret) {
            Ok(()) => {
                let _ = self.remove_from_file(account);
                Ok(Kept::Keychain)
            }
            Err(why) => {
                let Some(path) = &self.file else {
                    bail!(
                        "could not keep the token: the system keychain refused it ({why}), and there is no config \
                         directory for a file (set {FILE_VARIABLE} to a path)"
                    );
                };
                let mut contents = self.read_file()?;
                contents.insert(account.to_string(), saved.clone());
                write_private(path, &serde_json::to_string_pretty(&contents)?)?;
                Ok(Kept::File(path.clone()))
            }
        }
    }

    /// Removes the token kept for this qloudpulse, from the keychain and the file. Whether there was one.
    pub fn remove(&self, account: &str) -> Result<bool> {
        let in_keychain = self.keychain.delete(account).unwrap_or(false);
        let in_file = self.remove_from_file(account)?;
        Ok(in_keychain || in_file)
    }

    fn read_file(&self) -> Result<FileContents> {
        let Some(path) = &self.file else {
            return Ok(FileContents::new());
        };
        match std::fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => Ok(FileContents::new()),
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} is not the client's", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileContents::new()),
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    fn remove_from_file(&self, account: &str) -> Result<bool> {
        let Some(path) = &self.file else {
            return Ok(false);
        };
        let mut contents = self.read_file()?;
        if contents.remove(account).is_none() {
            return Ok(false);
        }
        if contents.is_empty() {
            std::fs::remove_file(path).with_context(|| format!("could not remove {}", path.display()))?;
        } else {
            write_private(path, &serde_json::to_string_pretty(&contents)?)?;
        }
        Ok(true)
    }
}

/// The credentials of the machine the client runs on: [`SystemKeychain`] with the file in the config directory, or
/// only the file [`FILE_VARIABLE`] names when it is set.
pub fn system() -> Credentials<Box<dyn Keychain>> {
    match std::env::var_os(FILE_VARIABLE).filter(|v| !v.is_empty()) {
        Some(file) => Credentials::new(Box::new(NoKeychain), Some(PathBuf::from(file))),
        None => Credentials::new(Box::new(SystemKeychain), default_file()),
    }
}

impl Keychain for Box<dyn Keychain> {
    fn get(&self, account: &str) -> Result<Option<String>, String> {
        (**self).get(account)
    }
    fn set(&self, account: &str, secret: &str) -> Result<(), String> {
        (**self).set(account, secret)
    }
    fn delete(&self, account: &str) -> Result<bool, String> {
        (**self).delete(account)
    }
}

/// `credentials` in the client's config directory: `%APPDATA%\qloudpulse` on Windows, else
/// `$XDG_CONFIG_HOME/qloudpulse` or `~/.config/qloudpulse`.
pub fn default_file() -> Option<PathBuf> {
    let absolute = |v: std::ffi::OsString| Some(PathBuf::from(v)).filter(|p| p.is_absolute());
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").and_then(absolute)?
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .and_then(absolute)
            .or_else(|| std::env::var_os("HOME").and_then(absolute).map(|h| h.join(".config")))?
    };
    Some(base.join("qloudpulse").join("credentials"))
}

/// Writes the file so that only its owner can read it: 0600 in a 0700 directory on Unix. On Windows the config
/// directory is the user's own already. The text goes to a file beside it first, so a crash leaves the old one.
fn write_private(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
            .create(dir)
            .with_context(|| format!("could not make {}", dir.display()))?;
    }
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let _ = std::fs::remove_file(&part);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&part)
        .with_context(|| format!("could not write {}", part.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .with_context(|| format!("could not write {}", part.display()))?;
    drop(file);
    std::fs::rename(&part, path).with_context(|| format!("could not write {}", path.display()))
}

/// Seconds since 1970 now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The date and time of an RFC 3339 timestamp, such as `2027-02-17T08:01:00Z` or `2027-02-17T09:01:00.5+01:00`.
fn parse(at: &str) -> Option<(i64, u32, u32, i64)> {
    let b = at.as_bytes();
    let digits = |r: std::ops::Range<usize>| -> Option<i64> {
        let s = at.get(r)?;
        s.bytes().all(|c| c.is_ascii_digit()).then(|| s.parse().ok())?
    };
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' {
        return None;
    }
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (hour, minute, second) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    if b[16] != b':' || !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let mut rest = &at[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let n = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if n == 0 {
            return None;
        }
        rest = &fraction[n..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 && matches!(rest.as_bytes()[0], b'+' | b'-') && rest.as_bytes()[3] == b':' => {
            let hours: i64 = rest[1..3].parse().ok()?;
            let minutes: i64 = rest[4..6].parse().ok()?;
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            sign * (hours * 3600 + minutes * 60)
        }
        _ => return None,
    };
    let seconds = hour * 3600 + minute * 60 + second.min(60) - offset;
    Some((year, u32::try_from(month).ok()?, u32::try_from(day).ok()?, seconds))
}

/// Days since 1970-01-01 of a date in the proleptic Gregorian calendar (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of days since 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Seconds since 1970 of an RFC 3339 timestamp.
pub fn unix_seconds(at: &str) -> Option<i64> {
    let (year, month, day, seconds) = parse(at)?;
    Some(days_from_civil(year, month, day) * 86_400 + seconds)
}

/// The day of an RFC 3339 timestamp in UTC, as people write it: `17 Feb 2027`.
pub fn day(at: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (year, month, day) = civil_from_days(unix_seconds(at)?.div_euclid(86_400));
    Some(format!("{day} {} {year}", MONTHS[month as usize - 1]))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;

    const TOKEN: &str = "qpev_acme.0a1b2c3d.abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
    const HOSTED: &str = "https://app.qloudpulse.com";

    /// A keychain in memory, or one that refuses everything, as a headless Linux machine's does.
    #[derive(Default)]
    struct Fake {
        entries: RefCell<HashMap<String, String>>,
        refuses: bool,
    }

    impl Keychain for Fake {
        fn get(&self, account: &str) -> Result<Option<String>, String> {
            if self.refuses {
                return Err("no Secret Service".into());
            }
            Ok(self.entries.borrow().get(account).cloned())
        }
        fn set(&self, account: &str, secret: &str) -> Result<(), String> {
            if self.refuses {
                return Err("no Secret Service".into());
            }
            self.entries.borrow_mut().insert(account.into(), secret.into());
            Ok(())
        }
        fn delete(&self, account: &str) -> Result<bool, String> {
            if self.refuses {
                return Err("no Secret Service".into());
            }
            Ok(self.entries.borrow_mut().remove(account).is_some())
        }
    }

    fn saved(expires_at: Option<&str>) -> Saved {
        Saved {
            token: TOKEN.into(),
            company: "acme".into(),
            expires_at: expires_at.map(str::to_string),
        }
    }

    /// A directory of its own under the system's temporary one, removed when dropped.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let dir = std::env::temp_dir().join(format!("qloudpulse-credentials-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Dir(dir)
        }
        fn file(&self) -> PathBuf {
            self.0.join("qloudpulse").join("credentials")
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const NOW: i64 = 1_795_000_000; // 2026-11-18

    #[test]
    fn timestamps_are_read_as_rfc_3339() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2027-02-17T08:01:00Z"), Some(1_802_851_260));
        assert_eq!(unix_seconds("2027-02-17T08:01:00.123456Z"), Some(1_802_851_260));
        assert_eq!(unix_seconds("2027-02-17T09:01:00+01:00"), Some(1_802_851_260));
        assert_eq!(unix_seconds("2024-02-29T00:00:00Z"), Some(1_709_164_800));
        for not in [
            "",
            "2027-02-17",
            "2027-02-17T08:01:00",
            "2027-13-17T08:01:00Z",
            "soon",
            "2027-02-17T08:01:00.Z",
        ] {
            assert_eq!(unix_seconds(not), None, "{not:?}");
        }
        assert_eq!(day("2027-02-17T08:01:00Z").as_deref(), Some("17 Feb 2027"));
        assert_eq!(day("2028-02-29T23:59:59Z").as_deref(), Some("29 Feb 2028"));
        assert_eq!(
            day("2027-01-01T00:30:00+01:00").as_deref(),
            Some("31 Dec 2026"),
            "the day in UTC"
        );
    }

    #[test]
    fn the_keychain_keeps_the_token_by_address() {
        let credentials = Credentials::new(Fake::default(), None);
        assert_eq!(credentials.load(HOSTED, NOW), None);
        let kept = credentials.save(HOSTED, &saved(Some("2027-02-17T08:01:00Z"))).unwrap();
        assert_eq!(kept, Kept::Keychain);
        assert_eq!(credentials.load(HOSTED, NOW), Some(saved(Some("2027-02-17T08:01:00Z"))));
        assert_eq!(
            credentials.load("https://qloudpulse.example.com", NOW),
            None,
            "another qloudpulse's is not it"
        );
        assert!(credentials.remove(HOSTED).unwrap());
        assert_eq!(credentials.load(HOSTED, NOW), None);
        assert!(!credentials.remove(HOSTED).unwrap(), "nothing left to remove");
    }

    #[test]
    fn an_expired_token_counts_as_none() {
        let credentials = Credentials::new(Fake::default(), None);
        credentials.save(HOSTED, &saved(Some("2026-11-01T00:00:00Z"))).unwrap();
        assert_eq!(credentials.load(HOSTED, NOW), None);
        credentials.save(HOSTED, &saved(None)).unwrap();
        assert!(
            credentials.load(HOSTED, NOW).is_some(),
            "without a date, qloudpulse decides"
        );
    }

    #[test]
    fn what_is_not_a_token_counts_as_none() {
        let credentials = Credentials::new(Fake::default(), None);
        credentials.keychain.set(HOSTED, "hunter2").unwrap();
        assert_eq!(credentials.load(HOSTED, NOW), None);
        credentials
            .keychain
            .set(HOSTED, r#"{"token": "qpev_x", "company": "acme"}"#)
            .unwrap();
        assert_eq!(credentials.load(HOSTED, NOW), None);
    }

    #[test]
    fn without_a_keychain_a_file_only_its_owner_reads() {
        let dir = Dir::new("file");
        let credentials = Credentials::new(
            Fake {
                refuses: true,
                ..Fake::default()
            },
            Some(dir.file()),
        );
        let kept = credentials.save(HOSTED, &saved(Some("2027-02-17T08:01:00Z"))).unwrap();
        assert_eq!(kept, Kept::File(dir.file()));
        assert_eq!(credentials.load(HOSTED, NOW), Some(saved(Some("2027-02-17T08:01:00Z"))));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir.file()), 0o600);
            assert_eq!(mode(dir.file().parent().unwrap()), 0o700);
        }

        credentials
            .save("https://qloudpulse.example.com", &saved(None))
            .unwrap();
        assert!(credentials.remove(HOSTED).unwrap());
        assert!(dir.file().exists(), "the other qloudpulse's token stays");
        assert_eq!(credentials.load(HOSTED, NOW), None);
        assert!(credentials.remove("https://qloudpulse.example.com").unwrap());
        assert!(!dir.file().exists(), "an empty file is removed");
    }

    /// The real keychain of the machine the test runs on, under an address no qloudpulse has. Not run by default, so
    /// `cargo test` never touches a developer's keychain: `cargo test -- --ignored the_system_keychain`.
    #[test]
    #[ignore = "uses this machine's keychain"]
    fn the_system_keychain_keeps_and_removes_a_token() {
        const ACCOUNT: &str = "https://selftest.invalid";
        let keychain = SystemKeychain;
        let _ = keychain.delete(ACCOUNT);
        assert_eq!(keychain.get(ACCOUNT), Ok(None));
        keychain.set(ACCOUNT, "secret").unwrap();
        assert_eq!(keychain.get(ACCOUNT), Ok(Some("secret".to_string())));
        assert_eq!(keychain.delete(ACCOUNT), Ok(true));
        assert_eq!(keychain.get(ACCOUNT), Ok(None));
        assert_eq!(keychain.delete(ACCOUNT), Ok(false));
    }

    #[test]
    fn once_the_keychain_holds_the_token_the_file_does_not() {
        let dir = Dir::new("move");
        let headless = Credentials::new(
            Fake {
                refuses: true,
                ..Fake::default()
            },
            Some(dir.file()),
        );
        headless.save(HOSTED, &saved(None)).unwrap();
        let desktop = Credentials::new(Fake::default(), Some(dir.file()));
        assert!(
            desktop.load(HOSTED, NOW).is_some(),
            "the file is read when the keychain has none"
        );
        assert_eq!(desktop.save(HOSTED, &saved(None)).unwrap(), Kept::Keychain);
        assert!(!dir.file().exists());
    }

    #[test]
    fn no_keychain_and_no_file_is_an_error_that_says_what_to_set() {
        let credentials = Credentials::new(
            Fake {
                refuses: true,
                ..Fake::default()
            },
            None,
        );
        let err = credentials.save(HOSTED, &saved(None)).unwrap_err().to_string();
        assert!(err.contains(FILE_VARIABLE) && !err.contains(TOKEN), "{err}");
    }
}
