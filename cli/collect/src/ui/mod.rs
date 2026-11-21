//! The local web UI: `qloudpulse-collect ui`, or `run --every 1h --ui`. See "Local UI" in
//! `docs/offline-collection.md`.
//!
//! It exists so people can see what leaves before it leaves: what the client reads, what the rules do with it, every
//! bundle it wrote and every run. It listens on 127.0.0.1 unless told otherwise, needs the token it prints, and
//! renders every page on the server; the little script it has only refreshes the rules preview. The raw sample is
//! held in memory and never written to disk.

mod diff;
mod pages;
mod rules;
mod server;
mod state;

pub use server::CSP;
pub use state::{Options, RunLog, Shared};

use crate::aws::UreqTransport;
use crate::aws::credentials::CredentialSource;
use crate::config::Config;
use crate::log;
use crate::run::{Client, next_run};
use anyhow::Result;
use jiff::Timestamp;
use state::{Cycle, Sample, lock};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 127.0.0.1 and a fixed port, so a bookmark keeps working. Another address needs `--ui-bind`.
pub const DEFAULT_BIND: &str = "127.0.0.1:7781";

/// Starts the UI and prints its address with the session token.
pub fn start(config_path: PathBuf, options: Options, run: Option<RunLog>) -> Result<Arc<Shared>> {
    let shared = server::start(config_path, options, run)?;
    let shown = if shared.bind.ip().is_unspecified() {
        format!("127.0.0.1:{}", shared.bind.port())
    } else {
        shared.bind.to_string()
    };
    println!("qloudpulse-collect UI: http://{shown}/?token={}", shared.token);
    println!("The token is new each time the UI starts; keep it to yourself.");
    if !shared.bind.ip().is_loopback() {
        println!(
            "Listening on {}: other machines can reach this UI. They still need the token.",
            shared.bind
        );
    }
    if !shared.allow_hosts.is_empty() {
        println!(
            "Also answers as {}. Put TLS in front of it: the token and the cookie travel in every request.",
            shared.allow_hosts.join(", ")
        );
    }
    if !shared.config_writable() {
        println!(
            "{} is on a read-only file system (a Kubernetes ConfigMap?): the rules editor offers the file to download \
             instead of saving it.",
            shared.config_path.display()
        );
    }
    Ok(shared)
}

/// `qloudpulse-collect ui`: the UI alone, until the process is stopped. `newer`, when given, says whether a newer
/// release is out; it runs beside the server, and what it says goes to stderr and to a line on every page.
pub fn serve<F>(config_path: PathBuf, options: Options, newer: Option<F>) -> Result<()>
where
    F: FnOnce() -> Option<String> + Send + 'static,
{
    let shared = start(config_path, options, None)?;
    if let Some(check) = newer {
        std::thread::spawn(move || {
            if let Some(line) = check() {
                eprintln!("qloudpulse-collect: {line}");
                *lock(&shared.newer) = Some(line);
            }
        });
    }
    loop {
        std::thread::park();
    }
}

/// The client `run` and the sample use: a fresh read of the config, so a saved rule applies from the next cycle.
pub fn client(config_path: &Path, every: Option<u32>) -> Result<Client> {
    let (mut config, config_bytes) = Config::load(config_path)?;
    if let Some(n) = every {
        config.run.every_hours = n;
    }
    let credentials = CredentialSource::new(config.profile.as_deref(), &config.home_region)?;
    Ok(Client {
        config,
        config_bytes,
        credentials,
        transport: Box::new(UreqTransport::new()),
        force_daily: false,
    })
}

/// `run --ui`: the UI, and the clock-aligned loop of `run` beside it. Each cycle reads the config again, so what the
/// rules editor saves applies from the next one; a config that no longer loads keeps the last good one running.
pub fn run_with_ui(config_path: PathBuf, every: Option<u32>, options: Options) -> Result<()> {
    let mut current = client(&config_path, every)?;
    current.pseudonyms()?;
    let shared = start(
        config_path.clone(),
        options,
        Some(RunLog {
            every_hours: current.config.run.every_hours,
            since: Timestamp::now(),
            next: None,
            cycles: Default::default(),
        }),
    )?;
    let run = shared.run.as_ref().expect("run --ui keeps a run log");
    loop {
        let now = Timestamp::now();
        let next = next_run(now, current.config.run.every_hours, current.config.run.minute);
        {
            let mut log_ = lock(run);
            log_.next = Some(next);
            log_.every_hours = current.config.run.every_hours;
        }
        log::info("waiting", &[("next", next.to_string())]);
        std::thread::sleep(std::time::Duration::try_from(next.duration_since(now)).unwrap_or_default());
        match client(&config_path, every) {
            Ok(c) => current = c,
            Err(e) => log::warn(
                "config not reloaded; the last good one runs",
                &[("error", format!("{e:#}"))],
            ),
        }
        // A sample being collected finishes first: the two never call AWS at once.
        let _collecting = lock(&shared.collector);
        let started = Timestamp::now();
        {
            let mut log_ = lock(run);
            log_.next = None;
            log_.cycles.push_front(Cycle {
                started,
                finished: None,
                bundle: None,
                error: None,
                pending: vec![],
            });
            log_.cycles.truncate(RunLog::KEEP);
        }
        let result = current.cycle(started);
        if let Err(e) = &result {
            log::warn("cycle failed", &[("error", format!("{e:#}"))]);
        }
        let pending = pending_targets(&current);
        let mut log_ = lock(run);
        if let Some(c) = log_.cycles.front_mut() {
            c.finished = Some(Timestamp::now());
            match result {
                Ok(path) => c.bundle = path.file_name().map(|n| n.to_string_lossy().into_owned()),
                Err(e) => c.error = Some(format!("{e:#}")),
            }
            c.pending = pending;
        }
    }
}

/// `bundle: target` for every delivery still owed.
fn pending_targets(client: &Client) -> Vec<String> {
    let state = pages::delivered(&client.config.output_dir);
    let wanted = crate::deliver::targets(&client.config);
    client
        .deliverer()
        .pending()
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .flat_map(|name| {
            let done = state.get(&name).cloned().unwrap_or_default();
            wanted
                .iter()
                .filter(move |t| !done.contains(**t))
                .map(move |t| format!("{name}: {t}"))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// "Collect sample now": reads the accounts on a thread of its own and keeps the records in memory.
pub(crate) fn start_sample(shared: Arc<Shared>) {
    {
        let mut sample = lock(&shared.sample);
        if matches!(*sample, Sample::Collecting { .. }) {
            return;
        }
        *sample = Sample::Collecting {
            since: Timestamp::now(),
        };
    }
    let spawned = std::thread::Builder::new().name("ui-sample".into()).spawn({
        let shared = shared.clone();
        move || {
            let result = {
                let _collecting = lock(&shared.collector);
                client(&shared.config_path, None).and_then(|c| c.collect_in_memory(Timestamp::now()))
            };
            let mut sample = lock(&shared.sample);
            // Forgotten while it ran: the answer is dropped too.
            if !matches!(*sample, Sample::Collecting { .. }) {
                return;
            }
            *sample = match result {
                Ok((meta, records)) => {
                    log::info("sample collected", &[("records", records.len().to_string())]);
                    Sample::Ready {
                        at: Timestamp::now(),
                        origin: "read live with your credentials".into(),
                        meta: Box::new(meta),
                        records,
                    }
                }
                Err(e) => Sample::Failed {
                    at: Timestamp::now(),
                    error: format!("{e:#}"),
                },
            };
        }
    });
    if let Err(e) = spawned {
        *lock(&shared.sample) = Sample::Failed {
            at: Timestamp::now(),
            error: e.to_string(),
        };
    }
}

/// A raw snapshot `collect` (or `run` with `keep_raw`) left in `work_dir/raw`, read into memory as the sample. The UI
/// only reads it: the file stays as it is, and nothing new is written.
pub(crate) fn load_sample(shared: &Shared, label: &str) -> Result<()> {
    if label.is_empty() || !label.chars().all(|c| c.is_ascii_alphanumeric()) {
        anyhow::bail!("{label} is not a snapshot name");
    }
    let (config, _) = Config::load(&shared.config_path)?;
    let snapshot = crate::snapshot::Snapshot::read(&config.work_dir.join("raw").join(label))?;
    *lock(&shared.sample) = Sample::Ready {
        at: Timestamp::now(),
        origin: format!("read from the raw snapshot {label} on disk"),
        meta: Box::new(snapshot.meta),
        records: snapshot.records,
    };
    Ok(())
}

pub(crate) fn forget_sample(shared: &Shared) {
    *lock(&shared.sample) = Sample::None;
}
