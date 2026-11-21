//! What the UI keeps while it runs: the session's secrets, the raw sample (memory only), and the cycles of the
//! `run` it sits in.

use crate::record::Record;
use crate::snapshot::SnapshotMeta;
use jiff::Timestamp;
use rand::RngCore;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

/// The raw sample behind "Collect sample now". It lives in this process only: nothing here is written to disk, and
/// it is gone when the UI stops or the user forgets it.
pub enum Sample {
    None,
    Collecting {
        since: Timestamp,
    },
    Ready {
        at: Timestamp,
        /// Where it came from: read live, or a raw snapshot `collect` left on disk.
        origin: String,
        meta: Box<SnapshotMeta>,
        records: Vec<Record>,
    },
    Failed {
        at: Timestamp,
        error: String,
    },
}

/// One cycle of the `run` loop this UI sits in.
pub struct Cycle {
    pub started: Timestamp,
    pub finished: Option<Timestamp>,
    pub bundle: Option<String>,
    pub error: Option<String>,
    /// Targets still missing a bundle after the cycle's deliveries: retried at the next cycle.
    pub pending: Vec<String>,
}

/// The `run` loop around the UI, when there is one (`run --ui`).
pub struct RunLog {
    pub every_hours: u32,
    pub since: Timestamp,
    pub next: Option<Timestamp>,
    /// Newest first, at most [`RunLog::KEEP`].
    pub cycles: VecDeque<Cycle>,
}

impl RunLog {
    pub const KEEP: usize = 200;
}

/// Where the UI listens and which names it answers to.
#[derive(Debug, Clone)]
pub struct Options {
    pub bind: SocketAddr,
    /// Host names it answers to besides addresses and `localhost`: an Ingress or a Service in front of it. Each is
    /// named by whoever starts the UI, so no page can add one.
    pub allow_hosts: Vec<String>,
}

impl Options {
    pub fn new(bind: SocketAddr, allow_hosts: Vec<String>) -> anyhow::Result<Options> {
        let mut hosts = Vec::new();
        for h in allow_hosts {
            let h = h.trim().trim_end_matches('.').to_ascii_lowercase();
            let label_ok = |l: &str| {
                !l.is_empty()
                    && l.len() <= 63
                    && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                    && !l.starts_with('-')
                    && !l.ends_with('-')
            };
            if h.len() > 253 || !h.split('.').all(label_ok) {
                anyhow::bail!(
                    "--ui-allow-host {h}: give a host name only, such as collect.example.com (no scheme, port or path)"
                );
            }
            hosts.push(h);
        }
        Ok(Options {
            bind,
            allow_hosts: hosts,
        })
    }
}

pub struct Shared {
    pub config_path: PathBuf,
    /// The address the server listens on, port included.
    pub bind: SocketAddr,
    /// Names from `--ui-allow-host`, lower case.
    pub allow_hosts: Vec<String>,
    /// Whether the config's directory takes a new file. A Kubernetes ConfigMap is mounted read-only: the rules editor
    /// then offers the file to download or copy instead of saving it.
    config_writable: AtomicBool,
    /// Printed at startup. The first request carries it once, as `?token=`, and gets it back as a cookie.
    pub token: String,
    /// Carried by every form: a POST without it is refused.
    pub csrf: String,
    pub sample: Mutex<Sample>,
    /// Held while the client reads AWS, by a sample or a cycle, so the two never call the APIs at once.
    pub collector: Mutex<()>,
    pub run: Option<Mutex<RunLog>>,
    /// A newer release of the client, when `ui` found one: shown on every page.
    pub newer: Mutex<Option<String>>,
}

impl Shared {
    pub fn new(config_path: PathBuf, bind: SocketAddr, allow_hosts: Vec<String>, run: Option<RunLog>) -> Shared {
        Shared {
            config_writable: AtomicBool::new(writable_dir(&config_path)),
            config_path,
            bind,
            allow_hosts,
            token: random_hex(32),
            csrf: random_hex(32),
            sample: Mutex::new(Sample::None),
            collector: Mutex::new(()),
            run: run.map(Mutex::new),
            newer: Mutex::new(None),
        }
    }

    /// `Host` must name this server by an IP address, `localhost`, or a name given with `--ui-allow-host`. Any other
    /// DNS name is refused, which is what stops DNS rebinding: a page on another site cannot reach the UI through a
    /// name it controls. The port is not checked: through `kubectl port-forward 8080:7781`, or a Service that maps a
    /// port, the browser names another port than the one the UI listens on.
    pub fn host_ok(&self, host: &str) -> bool {
        let name = host_name(host);
        if name.is_empty() {
            return false;
        }
        if self.allow_hosts.iter().any(|h| h.eq_ignore_ascii_case(name)) {
            return true;
        }
        let bind = self.bind.ip();
        if name.eq_ignore_ascii_case("localhost") {
            return bind.is_loopback() || bind.is_unspecified();
        }
        match name.parse::<std::net::IpAddr>() {
            Ok(ip) => bind.is_unspecified() || ip == bind || (ip.is_loopback() && bind.is_loopback()),
            Err(_) => false,
        }
    }

    pub fn config_writable(&self) -> bool {
        self.config_writable.load(Ordering::Relaxed)
    }

    /// A save failed because the file system refused it: from now on the editor offers the file instead.
    pub fn config_read_only(&self) {
        self.config_writable.store(false, Ordering::Relaxed);
    }
}

/// The host of a `Host` header (or an origin without its scheme): `[::1]:7781` -> `::1`, `example.com:443` ->
/// `example.com`, `localhost` -> `localhost`.
fn host_name(host: &str) -> &str {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split(']').next().unwrap_or("");
    }
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    }
}

/// Whether a file can be written next to `config_path`, the way the rules editor saves (a new file, then a rename).
fn writable_dir(config_path: &std::path::Path) -> bool {
    let dir = match config_path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => std::path::Path::new("."),
    };
    let probe = dir.join(format!(".qloudpulse-collect-ui-probe-{}", random_hex(4)));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Whether an error says the file system refused a write: read-only, or not ours to write.
pub fn refused_write(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
            )
        })
    })
}

pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut b);
    hex::encode(b)
}

/// Compares secrets in time that does not depend on where they differ.
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A lock that survives a panic in another holder: the UI shows what is there rather than failing every page.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(bind: &str) -> Shared {
        Shared::new("c.toml".into(), bind.parse().unwrap(), vec![], None)
    }

    #[test]
    fn host_must_be_an_address_or_an_allowed_name() {
        let s = shared("127.0.0.1:7781");
        assert!(s.host_ok("127.0.0.1:7781"));
        assert!(s.host_ok("localhost:7781"));
        assert!(s.host_ok("localhost:8080"), "kubectl port-forward 8080:7781");
        assert!(s.host_ok("127.0.0.1"));
        assert!(s.host_ok("[::1]:9000"), "another loopback address of the same machine");
        assert!(!s.host_ok("evil.example:7781"), "a DNS name is how rebinding gets in");
        assert!(!s.host_ok("localhost.evil.example:7781"));
        assert!(!s.host_ok("10.0.0.5:7781"));
        assert!(!s.host_ok(""));
        let v6 = shared("[::1]:7781");
        assert!(v6.host_ok("[::1]:7781"));
        let any = shared("0.0.0.0:7781");
        assert!(any.host_ok("10.0.0.5:7781"));
        assert!(any.host_ok("10.96.12.7"), "a Service's cluster IP on port 80");
        assert!(!any.host_ok("collect.example.com"));
        let o = Options::new("0.0.0.0:7781".parse().unwrap(), vec!["Collect.Example.com".into()]).unwrap();
        let named = Shared::new("c.toml".into(), o.bind, o.allow_hosts, None);
        assert!(named.host_ok("collect.example.com"));
        assert!(named.host_ok("COLLECT.example.com:443"));
        assert!(!named.host_ok("other.example.com"));
        assert!(!named.host_ok("collect.example.com.evil.example"));
    }

    #[test]
    fn allowed_hosts_are_names_only() {
        let bind: SocketAddr = "0.0.0.0:7781".parse().unwrap();
        for bad in [
            "https://collect.example.com",
            "collect.example.com:443",
            "collect.example.com/ui",
            "*.example.com",
            "",
        ] {
            assert!(Options::new(bind, vec![bad.into()]).is_err(), "{bad}");
        }
        assert_eq!(
            Options::new(bind, vec!["qpc.ns.svc.".into()]).unwrap().allow_hosts,
            vec!["qpc.ns.svc"]
        );
    }

    #[test]
    fn a_read_only_directory_is_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("c.toml");
        assert!(writable_dir(&config));
        assert!(
            std::fs::read_dir(dir.path()).unwrap().next().is_none(),
            "the probe is gone"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
            // root writes anyway; the check is only meaningful for anyone else.
            if std::fs::write(dir.path().join("x"), "").is_err() {
                assert!(!writable_dir(&config));
            }
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let e = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::ReadOnlyFilesystem))
            .context("replacing c.toml");
        assert!(refused_write(&e));
        assert!(!refused_write(&anyhow::anyhow!("the config changed on disk")));
    }

    #[test]
    fn secrets_compare() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
        assert_eq!(random_hex(32).len(), 64);
    }
}
