//! One cycle (collect, pack, deliver, prune) and the clock-aligned loop around it.

use crate::aws::caller::Account;
use crate::aws::credentials::CredentialSource;
use crate::aws::{Caller, Transport};
use crate::bundle;
use crate::callplan::CallPlan;
use crate::collect::{self, Run};
use crate::config::Config;
use crate::deliver::Deliverer;
use crate::log;
use crate::pseudonym::Pseudonymizer;
use crate::snapshot::{Snapshot, SnapshotMeta};
use crate::window::Window;
use crate::window::{HOUR, trunc_hour, utc_hour};
use anyhow::{Context, Result};
use jiff::{SignedDuration, Timestamp};
use std::path::PathBuf;

pub struct Client {
    pub config: Config,
    pub config_bytes: Vec<u8>,
    pub credentials: CredentialSource,
    pub transport: Box<dyn Transport>,
    /// Read the daily parts whatever the hour (`collect --daily`).
    pub force_daily: bool,
    /// Told each time the work moves on. The local UI shows it; `run` without the UI has none and only logs.
    pub progress: Option<Progress>,
}

pub type Progress = Box<dyn Fn(Phase) + Send + Sync>;

/// Where a cycle (or the UI's sample) is. Counts and region names only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Waiting for the other reader of AWS in this process: a sample for a cycle, a cycle for a sample.
    Queued,
    /// Assuming the roles and listing the regions.
    Starting,
    /// Reading one account in one region; both are counted from 1.
    Collecting {
        /// What is being read: the hourly inventory, then ECS.
        part: &'static str,
        account: usize,
        accounts: usize,
        region: usize,
        regions: usize,
        region_name: String,
        /// Answers recorded so far.
        records: usize,
    },
    /// A daily part: the bill, the forecast, carbon.
    Reading(&'static str),
    Packing,
    Delivering,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Phase::Queued => f.write_str("waiting for the other collection in this process to finish"),
            Phase::Starting => f.write_str("starting: assuming roles and listing regions"),
            Phase::Collecting {
                part,
                account,
                accounts,
                region,
                regions,
                region_name,
                records,
            } => write!(
                f,
                "collecting {part}: account {account} of {accounts}, region {region} of {regions} ({region_name}), \
                 {records} records so far"
            ),
            Phase::Reading(what) => write!(f, "reading {what}"),
            Phase::Packing => f.write_str("packing the bundle"),
            Phase::Delivering => f.write_str("delivering"),
        }
    }
}

/// What a turn of the `run` loop did.
#[derive(Debug, PartialEq, Eq)]
pub enum Turn {
    /// Collected and packed this bundle.
    Packed(PathBuf),
    /// The window's bundle was already in the output directory (this process started again within the hour): nothing
    /// was collected. Deliveries still owed were tried.
    AlreadyCollected(PathBuf),
}

impl Turn {
    pub fn bundle(&self) -> &PathBuf {
        match self {
            Turn::Packed(p) | Turn::AlreadyCollected(p) => p,
        }
    }
}

impl Client {
    fn at(&self, phase: Phase) {
        if let Some(tell) = &self.progress {
            tell(phase);
        }
    }

    /// Reads every configured account and region into a raw snapshot.
    pub fn collect(&self, now: Timestamp) -> Result<Snapshot> {
        let (meta, records) = self.collect_in_memory(now)?;
        let snapshot = Snapshot::write(Snapshot::dir_for(&self.config.work_dir, &meta.window), meta, records)?;
        log::info(
            "collected",
            &[
                ("records", snapshot.records.len().to_string()),
                ("snapshot", snapshot.dir.display().to_string()),
            ],
        );
        Ok(snapshot)
    }

    /// The same reading as [`Client::collect`], kept in memory: the local UI's sample never touches the disk.
    pub fn collect_in_memory(&self, now: Timestamp) -> Result<(SnapshotMeta, Vec<crate::record::Record>)> {
        let window = Window::last_full_hour(now);
        let mut caller = Caller::new(CallPlan::embedded(), self.transport.as_ref(), self.config.options());
        // As the AWS SDKs (and aws-config, for STS) honour it: everything goes to that endpoint instead of AWS.
        caller.endpoint = std::env::var("AWS_ENDPOINT_URL").ok().filter(|e| !e.trim().is_empty());
        let home = &self.config.home_region;
        self.at(Phase::Starting);
        let mut failures = Vec::new();
        let mut accounts = Vec::new();
        if self.config.accounts.is_empty() {
            let credentials = self.credentials.ambient()?;
            let id =
                collect::caller_account(&caller, &credentials, home).context("finding the credentials' account")?;
            accounts.push(Account { id, credentials });
        } else {
            for a in &self.config.accounts {
                match self.credentials.assume(&a.role_arn, a.external_id.as_deref()) {
                    Ok(credentials) => accounts.push(Account {
                        id: a.id.clone(),
                        credentials,
                    }),
                    Err(e) => {
                        log::warn(
                            "account skipped",
                            &[("account", a.id.clone()), ("error", format!("{e:#}"))],
                        );
                        failures.push(format!("{}: could not assume {}: {e:#}", a.id, a.role_arn));
                    }
                }
            }
        }
        anyhow::ensure!(!accounts.is_empty(), "no account could be read");
        let regions = match &self.config.regions {
            Some(r) => r.clone(),
            None => collect::enabled_regions(&caller, &accounts[0], home).context("listing enabled regions")?,
        };
        let daily = self.force_daily || utc_hour(now) == self.config.run.daily_hour;
        let run = Run {
            caller: &caller,
            window,
            spot_history: daily,
            progress: self.progress.as_deref(),
        };
        log::info(
            "collecting",
            &[
                ("window", window.label()),
                ("accounts", accounts.len().to_string()),
                ("regions", regions.len().to_string()),
            ],
        );
        let mut outcome = run.collect(&accounts, &regions);
        if daily && let Some(c) = &self.config.cur {
            match self.payer(c, &accounts) {
                Err(e) => failures.push(format!("{} {} cur: {e:#}", c.account, c.region)),
                Ok(payer) => {
                    self.at(Phase::Reading("the bill"));
                    match self.read_bill(&caller, c, &payer, now) {
                        Ok((cur, rows)) => {
                            outcome.cur = Some(cur);
                            self.at(Phase::Reading("the resources on the bill"));
                            let today = now.to_zoned(jiff::tz::TimeZone::UTC).date().to_string();
                            for account in &accounts {
                                if let Err(e) = collect::infra::collect(&run, account, &rows, &today) {
                                    failures.push(format!("{} infra: {e:#}", account.id));
                                }
                            }
                        }
                        Err(e) => {
                            log::warn(
                                "bill not read",
                                &[("account", c.account.clone()), ("error", format!("{e:#}"))],
                            );
                            failures.push(format!("{} {} cur: {e:#}", c.account, c.region));
                        }
                    }
                    if c.forecast {
                        self.at(Phase::Reading("the forecast"));
                        match collect::forecast::collect(&caller, &payer, now, collect::forecast::PREDICTION_INTERVAL) {
                            Ok(f) => outcome.forecast = Some(f),
                            Err(e) => failures.push(format!("{} forecast: {e:#}", c.account)),
                        }
                    }
                }
            }
        }
        if daily && self.config.carbon {
            self.at(Phase::Reading("the carbon estimate"));
            for account in &accounts {
                if let Err(e) = collect::carbon::collect(&caller, account, now) {
                    failures.push(format!("{} carbon: {e:#}", account.id));
                }
            }
        }
        outcome.failures.splice(0..0, failures);
        let records = caller.into_records();
        let meta = SnapshotMeta {
            tool_version: env!("CARGO_PKG_VERSION").into(),
            call_plan_version: CallPlan::embedded().version.clone(),
            window,
            collected_at: now,
            outcome,
        };
        Ok((meta, records))
    }

    /// The payer account's credentials: the `[cur]` role when set, else the payer among the accounts read.
    fn payer(&self, c: &crate::config::CurConfig, accounts: &[Account]) -> Result<Account> {
        match &c.role_arn {
            Some(role) => Ok(Account {
                id: c.account.clone(),
                credentials: self.credentials.assume(role, c.external_id.as_deref())?,
            }),
            None => accounts
                .iter()
                .find(|a| a.id == c.account)
                .cloned()
                .context("the payer account is not among the accounts read; set [cur] role_arn"),
        }
    }

    fn read_bill(
        &self,
        caller: &Caller,
        c: &crate::config::CurConfig,
        payer: &Account,
        now: Timestamp,
    ) -> Result<(collect::cur::CurOutcome, Vec<collect::cur::BillRow>)> {
        let settings = collect::cur::CurSettings {
            region: c.region.clone(),
            workgroup: c.workgroup.clone(),
            database: c.database.clone(),
            table: c.table.clone(),
            scope_tag_keys: c.scope_tag_keys.clone(),
            periods: collect::cur::periods(now, c.periods_back),
            first_poll: std::time::Duration::from_secs(1),
        };
        log::info("reading the bill", &[("periods", settings.periods.join(","))]);
        collect::cur::collect(caller, payer, &settings)
    }

    pub fn pseudonyms(&self) -> Result<Pseudonymizer> {
        Pseudonymizer::load(&self.config.key_file, self.config.keep_pseudonym_map)
    }

    pub fn pack(&self, snapshot: &Snapshot, allow_findings: bool) -> Result<PathBuf> {
        let pseudonyms = self.pseudonyms()?;
        let path = bundle::pack(&self.config, &self.config_bytes, &pseudonyms, snapshot, allow_findings)?;
        log::info("packed", &[("bundle", path.display().to_string())]);
        for line in self.config.feature_impact() {
            log::warn("rule cost", &[("impact", line)]);
        }
        Ok(path)
    }

    pub fn deliverer(&self) -> Deliverer<'_> {
        Deliverer {
            config: &self.config,
            transport: self.transport.as_ref(),
            credentials: Some(&self.credentials),
        }
    }

    /// Collect, pack, drop the raw snapshot (unless kept), deliver everything pending, prune.
    pub fn cycle(&self, now: Timestamp) -> Result<PathBuf> {
        let snapshot = self.collect(now)?;
        self.at(Phase::Packing);
        // A bundle the scan stops is not written, and its raw snapshot is kept so you can look at it.
        let bundle = self
            .pack(&snapshot, false)
            .with_context(|| format!("raw snapshot kept in {}", snapshot.dir.display()))?;
        if !self.config.run.keep_raw {
            std::fs::remove_dir_all(&snapshot.dir).with_context(|| format!("removing {}", snapshot.dir.display()))?;
        }
        self.deliver_pending()?;
        Ok(bundle)
    }

    /// Delivers every bundle a target still lacks, then prunes.
    fn deliver_pending(&self) -> Result<()> {
        self.at(Phase::Delivering);
        let deliverer = self.deliverer();
        for pending in deliverer.pending()? {
            // Each failure is logged and retried next cycle.
            let _ = deliverer.deliver(&pending, None);
        }
        let removed = deliverer.prune()?;
        if removed > 0 {
            log::info("pruned", &[("bundles", removed.to_string())]);
        }
        Ok(())
    }

    /// Where the bundle of `now`'s window is written.
    pub fn bundle_path(&self, now: Timestamp) -> PathBuf {
        self.config
            .output_dir
            .join(bundle::file_name(&self.config.source_id, &Window::last_full_hour(now)))
    }

    /// A cycle of the `run` loop: [`Client::cycle`], unless the window's bundle is already in the output directory.
    /// A process that starts again within the hour (a pod that restarts) then collects nothing a second time; it
    /// still delivers what is owed.
    pub fn turn(&self, now: Timestamp) -> Result<Turn> {
        let path = self.bundle_path(now);
        if !path.is_file() {
            return self.cycle(now).map(Turn::Packed);
        }
        log::info(
            "already collected",
            &[
                ("window", Window::last_full_hour(now).label()),
                ("bundle", path.display().to_string()),
            ],
        );
        self.deliver_pending()?;
        Ok(Turn::AlreadyCollected(path))
    }

    /// Runs a cycle when `run` starts, then one at each aligned time, forever. A failed cycle is logged and the loop
    /// goes on.
    pub fn run_forever(&self) -> ! {
        let mut last = None;
        loop {
            let now = Timestamp::now();
            let next = due(now, last, self.config.run.every_hours, self.config.run.minute);
            if next > now {
                log::info("waiting", &[("next", next.to_string())]);
                std::thread::sleep(std::time::Duration::try_from(next.duration_since(now)).unwrap_or_default());
            }
            let started = Timestamp::now();
            last = Some(started);
            if let Err(e) = self.turn(started) {
                log::warn("cycle failed", &[("error", format!("{e:#}"))]);
            }
        }
    }
}

/// When the `run` loop's next cycle starts. `last` is when its previous one started; `None` when `run` has just
/// started.
///
/// - Just started: now. Before `minute` past the hour the hour that just ended may still be incomplete at AWS, so
///   the cycle waits for that minute; it is at most `minute` minutes away.
/// - After a cycle: the first aligned time after that cycle *started*. A cycle that ran past an aligned time is
///   followed at once by the next, so no window is left out. The window a cycle reads is the last full hour before
///   its start, and two starts that [`next_run`] apart are in different hours, so no window is read twice.
pub fn due(now: Timestamp, last: Option<Timestamp>, every_hours: u32, minute: u32) -> Timestamp {
    match last {
        Some(started) => next_run(started, every_hours, minute),
        None => now.max(trunc_hour(now) + SignedDuration::from_mins(minute as i64)),
    }
}

/// The next time after `now` at `minute` past an hour divisible by `every_hours`, in UTC.
pub fn next_run(now: Timestamp, every_hours: u32, minute: u32) -> Timestamp {
    let mut t = trunc_hour(now) + SignedDuration::from_mins(minute as i64);
    while t <= now || !utc_hour(t).is_multiple_of(every_hours) {
        t += HOUR;
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn aligns_to_the_clock() {
        assert_eq!(next_run(at("2026-11-21T07:03:00Z"), 1, 5), at("2026-11-21T07:05:00Z"));
        assert_eq!(next_run(at("2026-11-21T07:05:00Z"), 1, 5), at("2026-11-21T08:05:00Z"));
        assert_eq!(next_run(at("2026-11-21T07:30:00Z"), 6, 5), at("2026-11-21T12:05:00Z"));
    }

    #[test]
    fn run_starts_with_a_cycle_and_never_repeats_a_window() {
        // Started at 17:27: a cycle at once, for 16:00-17:00, then 18:05 for 17:00-18:00.
        let start = at("2026-11-26T17:27:00Z");
        assert_eq!(due(start, None, 1, 5), start);
        let next = due(at("2026-11-26T17:29:00Z"), Some(start), 1, 5);
        assert_eq!(next, at("2026-11-26T18:05:00Z"));
        assert_ne!(Window::last_full_hour(start), Window::last_full_hour(next));
        // Started before the minute: the hour that just ended is not complete yet, so wait for the minute.
        let early = at("2026-11-26T18:02:00Z");
        let first = due(early, None, 1, 5);
        assert_eq!(first, at("2026-11-26T18:05:00Z"));
        assert_eq!(
            due(first, Some(first), 1, 5),
            at("2026-11-26T19:05:00Z"),
            "not 18:05 again"
        );
        // With --every 6h the cycle at the start is the only one off the grid.
        assert_eq!(due(start, None, 6, 5), start);
        assert_eq!(due(start, Some(start), 6, 5), at("2026-11-26T18:05:00Z"));
        assert_eq!(
            due(start, Some(at("2026-11-26T18:05:00Z")), 6, 5),
            at("2026-11-27T00:05:00Z")
        );
        // A cycle that started at 17:59 and ran past 18:05 is followed at once: 17:00-18:00 is not left out.
        let late = due(at("2026-11-26T18:07:00Z"), Some(at("2026-11-26T17:59:00Z")), 1, 5);
        assert!(late <= at("2026-11-26T18:07:00Z"));
    }

    /// Fails the test if anything is sent.
    struct Silent;

    impl Transport for Silent {
        fn send(&self, request: &crate::aws::transport::HttpRequest) -> Result<crate::aws::transport::HttpResponse> {
            panic!("nothing may be called, but {} was", request.url)
        }
    }

    #[test]
    fn a_window_already_bundled_is_not_collected_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut config: Config = toml::from_str("source_id = \"main\"").unwrap();
        config.output_dir = dir.path().join("bundles");
        config.work_dir = dir.path().join("work");
        std::fs::create_dir_all(&config.output_dir).unwrap();
        let client = Client {
            credentials: CredentialSource::new(None, "eu-west-1").unwrap(),
            config,
            config_bytes: vec![],
            transport: Box::new(Silent),
            force_daily: false,
            progress: None,
        };
        // The pod started at 17:27, packed 16:00-17:00, and starts again at 17:40.
        let now = at("2026-11-26T17:40:00Z");
        let bundle = client.bundle_path(now);
        assert!(bundle.ends_with("qloudpulse-main-20261126T1600Z.tgz"), "{bundle:?}");
        std::fs::write(&bundle, b"packed at 17:27").unwrap();
        assert_eq!(client.turn(now).unwrap(), Turn::AlreadyCollected(bundle.clone()));
        assert_eq!(std::fs::read(&bundle).unwrap(), b"packed at 17:27", "left as it was");
        // An hour later the window is another one, and its bundle is not there.
        assert!(!client.bundle_path(at("2026-11-26T18:05:00Z")).exists());
    }

    #[test]
    fn phases_read_as_a_sentence() {
        let p = Phase::Collecting {
            part: "inventory",
            account: 1,
            accounts: 2,
            region: 3,
            regions: 17,
            region_name: "eu-west-1".into(),
            records: 412,
        };
        assert_eq!(
            p.to_string(),
            "collecting inventory: account 1 of 2, region 3 of 17 (eu-west-1), 412 records so far"
        );
        assert_eq!(Phase::Reading("the bill").to_string(), "reading the bill");
    }
}
