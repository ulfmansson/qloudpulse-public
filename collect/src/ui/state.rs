//! What the UI keeps while it runs: the session's secrets, the raw sample (memory only), and the cycles of the
//! `run` it sits in.

use crate::record::Record;
use crate::snapshot::SnapshotMeta;
use jiff::Timestamp;
use rand::RngCore;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
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

pub struct Shared {
    pub config_path: PathBuf,
    /// The address the server listens on, port included.
    pub bind: SocketAddr,
    /// Printed at startup. The first request carries it once, as `?token=`, and gets it back as a cookie.
    pub token: String,
    /// Carried by every form: a POST without it is refused.
    pub csrf: String,
    pub sample: Mutex<Sample>,
    /// Held while the client reads AWS, by a sample or a cycle, so the two never call the APIs at once.
    pub collector: Mutex<()>,
    pub run: Option<Mutex<RunLog>>,
}

impl Shared {
    pub fn new(config_path: PathBuf, bind: SocketAddr, run: Option<RunLog>) -> Shared {
        Shared {
            config_path,
            bind,
            token: random_hex(32),
            csrf: random_hex(32),
            sample: Mutex::new(Sample::None),
            collector: Mutex::new(()),
            run: run.map(Mutex::new),
        }
    }

    /// `Host` must name this server by an IP address or `localhost`, with its port. A DNS name is refused, which is
    /// what stops DNS rebinding: a page on another site cannot reach the UI through a name it controls.
    pub fn host_ok(&self, host: &str) -> bool {
        let Some((name, port)) = host.rsplit_once(':') else {
            return false;
        };
        if port.parse::<u16>().ok() != Some(self.bind.port()) {
            return false;
        }
        let name = name.trim_start_matches('[').trim_end_matches(']');
        if name.eq_ignore_ascii_case("localhost") {
            return self.bind.ip().is_loopback() || self.bind.ip().is_unspecified();
        }
        match name.parse::<std::net::IpAddr>() {
            Ok(ip) => self.bind.ip().is_unspecified() || ip == self.bind.ip(),
            Err(_) => false,
        }
    }
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
        Shared::new("c.toml".into(), bind.parse().unwrap(), None)
    }

    #[test]
    fn host_must_be_an_address_with_our_port() {
        let s = shared("127.0.0.1:7781");
        assert!(s.host_ok("127.0.0.1:7781"));
        assert!(s.host_ok("localhost:7781"));
        assert!(!s.host_ok("127.0.0.1:80"));
        assert!(!s.host_ok("127.0.0.1"));
        assert!(!s.host_ok("evil.example:7781"), "a DNS name is how rebinding gets in");
        assert!(!s.host_ok("10.0.0.5:7781"));
        let v6 = shared("[::1]:7781");
        assert!(v6.host_ok("[::1]:7781"));
        let any = shared("0.0.0.0:7781");
        assert!(any.host_ok("10.0.0.5:7781"));
        assert!(!any.host_ok("collect.example.com:7781"));
    }

    #[test]
    fn secrets_compare() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
        assert_eq!(random_hex(32).len(), 64);
    }
}
