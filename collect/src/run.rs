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
}

impl Client {
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
        let caller = Caller::new(CallPlan::embedded(), self.transport.as_ref(), self.config.options());
        let home = &self.config.home_region;
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
                    match self.read_bill(&caller, c, &payer, now) {
                        Ok((cur, rows)) => {
                            outcome.cur = Some(cur);
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
                        match collect::forecast::collect(&caller, &payer, now, c.prediction_interval) {
                            Ok(f) => outcome.forecast = Some(f),
                            Err(e) => failures.push(format!("{} forecast: {e:#}", c.account)),
                        }
                    }
                }
            }
        }
        if daily && self.config.carbon {
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
        // A bundle the scan stops is not written, and its raw snapshot is kept so you can look at it.
        let bundle = self
            .pack(&snapshot, false)
            .with_context(|| format!("raw snapshot kept in {}", snapshot.dir.display()))?;
        if !self.config.run.keep_raw {
            std::fs::remove_dir_all(&snapshot.dir).with_context(|| format!("removing {}", snapshot.dir.display()))?;
        }
        let deliverer = self.deliverer();
        for pending in deliverer.pending()? {
            // Each failure is logged and retried next cycle.
            let _ = deliverer.deliver(&pending, None);
        }
        let removed = deliverer.prune()?;
        if removed > 0 {
            log::info("pruned", &[("bundles", removed.to_string())]);
        }
        Ok(bundle)
    }

    /// Runs a cycle at each aligned time, forever. A failed cycle is logged and the loop goes on.
    pub fn run_forever(&self) -> ! {
        loop {
            let now = Timestamp::now();
            let next = next_run(now, self.config.run.every_hours, self.config.run.minute);
            log::info("waiting", &[("next", next.to_string())]);
            std::thread::sleep(std::time::Duration::try_from(next.duration_since(now)).unwrap_or_default());
            if let Err(e) = self.cycle(Timestamp::now()) {
                log::warn("cycle failed", &[("error", format!("{e:#}"))]);
            }
        }
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
}
