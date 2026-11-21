use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use qloudpulse_collect::aws::UreqTransport;
use qloudpulse_collect::aws::credentials::CredentialSource;
use qloudpulse_collect::callplan::CallPlan;
use qloudpulse_collect::config::{Config, starter};
use qloudpulse_collect::pseudonym::Pseudonymizer;
use qloudpulse_collect::run::Client;
use qloudpulse_collect::snapshot::Snapshot;
use qloudpulse_collect::{analyze, bundle, deliver, log};
use std::path::{Path, PathBuf};

/// Collects cloud cost data with your own credentials into a bundle you inspect, redact and deliver.
///
/// Nothing is read from qloudpulse. The only time the client contacts qloudpulse is `upload`, or `run` with
/// [deliver.qloudpulse] configured, and then it sends a finished bundle and nothing else.
#[derive(Parser)]
#[command(version = env!("QLOUDPULSE_LONG_VERSION"), name = "qloudpulse-collect")]
struct Cli {
    /// The config file.
    #[arg(
        long,
        global = true,
        env = "QLOUDPULSE_COLLECT_CONFIG",
        default_value = "qloudpulse-collect.toml"
    )]
    config: PathBuf,
    /// Log one JSON object per line, for unattended runs.
    #[arg(long, global = true)]
    log_json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a starter config and create the pseudonym key.
    Init,
    /// Read the accounts into a raw snapshot (local only, never delivered).
    Collect {
        /// Also read the daily parts (the bill, the 90-day spot history) whatever the hour.
        #[arg(long)]
        daily: bool,
    },
    /// Show what a raw snapshot or a bundle holds: tag keys, values, what the rules do, what they cost.
    Analyze {
        /// A raw snapshot directory or a bundle (.tgz). The newest raw snapshot when absent.
        path: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        /// Leave tag values out of the report.
        #[arg(long)]
        no_values: bool,
        /// Print a [tags] section listing every key found, for the config.
        #[arg(long)]
        draft_rules: bool,
    },
    /// Apply the config to a raw snapshot and write the bundle.
    Pack {
        /// The newest raw snapshot when absent.
        snapshot: Option<PathBuf>,
        /// Write the bundle even when the scan finds keys, tokens or personal data in it.
        #[arg(long)]
        allow_findings: bool,
    },
    /// Check a bundle against its manifest and summarize it.
    Verify { bundle: PathBuf },
    /// Send bundles to the configured targets: every pending one when none is named.
    Deliver {
        bundles: Vec<PathBuf>,
        /// Only these targets: s3, qloudpulse, command.
        #[arg(long = "to")]
        to: Vec<String>,
    },
    /// Upload bundles to qloudpulse, signed in as the qloudpulse CLI is: QLOUDPULSE_TOKEN, else the token `login` kept.
    #[cfg(feature = "upload")]
    Upload {
        #[arg(required = true)]
        bundles: Vec<PathBuf>,
        /// The qloudpulse API; [deliver.qloudpulse] endpoint, or qloudpulse's own.
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// Sign in with the browser and keep a token on this machine, as `qloudpulse login` does (they share it).
    #[cfg(feature = "upload")]
    Login {
        #[arg(long, default_value = qloudpulse_collect::config::QLOUDPULSE_ENDPOINT)]
        endpoint: String,
    },
    /// Remove the token kept on this machine. It still works until it expires or you remove it under Your tokens.
    #[cfg(feature = "upload")]
    Logout {
        #[arg(long, default_value = qloudpulse_collect::config::QLOUDPULSE_ENDPOINT)]
        endpoint: String,
    },
    /// Collect, pack and deliver every N hours, on the clock.
    Run {
        /// One cycle now, then exit.
        #[arg(long)]
        once: bool,
        /// Overrides [run] every_hours.
        #[arg(long)]
        every: Option<u32>,
        /// Also serve the local UI, with this loop's cycles in its run history.
        #[arg(long, conflicts_with = "once")]
        ui: bool,
        /// Where the UI listens. Anything but a loopback address lets other machines reach it (the token is still
        /// required).
        #[arg(long, default_value = qloudpulse_collect::ui::DEFAULT_BIND, requires = "ui")]
        ui_bind: std::net::SocketAddr,
    },
    /// The local web UI: what the client reads, the rules editor with a raw/redacted preview, every bundle, every
    /// run. Listens on 127.0.0.1 and prints an address with a session token.
    Ui {
        /// Where the UI listens. Anything but a loopback address lets other machines reach it (the token is still
        /// required).
        #[arg(long, default_value = qloudpulse_collect::ui::DEFAULT_BIND)]
        ui_bind: std::net::SocketAddr,
    },
    /// Replace the pseudonym key. Writes an old-to-new pseudonym file (no real values) to relink history in
    /// qloudpulse, from the local map (keep_pseudonym_map); without one it refuses unless --force.
    RotateKey {
        #[arg(long)]
        force: bool,
    },
    /// Print the read-only IAM policy the call plan needs (plus S3 delivery, if configured).
    Policy,
    /// Print the compiled-in call plan: every operation the client may call.
    CallPlan,
}

fn main() {
    let cli = Cli::parse();
    log::use_json(cli.log_json);
    if let Err(e) = run(cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn load(path: &Path) -> Result<(Config, Vec<u8>)> {
    Config::load(path).with_context(|| {
        format!(
            "loading the config (create one with `qloudpulse-collect init --config {}`)",
            path.display()
        )
    })
}

fn client(path: &Path) -> Result<Client> {
    let (config, config_bytes) = load(path)?;
    let credentials = CredentialSource::new(config.profile.as_deref(), &config.home_region)?;
    Ok(Client {
        config,
        config_bytes,
        credentials,
        transport: Box::new(UreqTransport::new()),
        force_daily: false,
    })
}

fn newest_snapshot(config: &Config) -> Result<PathBuf> {
    let raw = config.work_dir.join("raw");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&raw)
        .with_context(|| format!("no raw snapshots in {}; run `collect` first", raw.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("snapshot.json").exists())
        .collect();
    dirs.sort();
    dirs.pop()
        .with_context(|| format!("no raw snapshots in {}; run `collect` first", raw.display()))
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init => {
            if cli.config.exists() {
                println!("{} exists; left as it is", cli.config.display());
            } else {
                std::fs::write(&cli.config, starter())?;
                println!("wrote {}", cli.config.display());
            }
            let (config, _) = load(&cli.config)?;
            if config.key_file.exists() {
                println!("{} exists; left as it is", config.key_file.display());
            } else {
                Pseudonymizer::create_key(&config.key_file)?;
                println!(
                    "created the pseudonym key {}. Back it up: losing it changes every pseudonym.",
                    config.key_file.display()
                );
            }
        }
        Command::Collect { daily } => {
            let mut client = client(&cli.config)?;
            client.force_daily = daily;
            let snapshot = client.collect(jiff::Timestamp::now())?;
            println!("{}", snapshot.dir.display());
        }
        Command::Analyze {
            path,
            json,
            no_values,
            draft_rules,
        } => {
            let config = load(&cli.config).ok().map(|(c, _)| c);
            let path = match path {
                Some(p) => p,
                None => newest_snapshot(
                    config
                        .as_ref()
                        .context("give a path, or a config to find the newest snapshot")?,
                )?,
            };
            let (source, records) = if path.is_dir() {
                ("raw snapshot (not delivered as is)", Snapshot::read(&path)?.records)
            } else {
                ("bundle (exactly what leaves)", bundle::open(&path)?.records)
            };
            let report = analyze::analyze(source, &records, config.as_ref(), !no_values);
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if draft_rules {
                print!("{}", report.draft_rules());
            } else {
                print!("{}", report.text());
            }
        }
        Command::Pack {
            snapshot,
            allow_findings,
        } => {
            let client = client(&cli.config)?;
            let dir = match snapshot {
                Some(d) => d,
                None => newest_snapshot(&client.config)?,
            };
            let path = client.pack(&Snapshot::read(&dir)?, allow_findings)?;
            println!("{}", path.display());
        }
        Command::Verify { bundle: path } => {
            let opened = bundle::open(&path)?;
            let m = &opened.manifest;
            println!("{}: every file matches the manifest", path.display());
            println!("source {} window {} .. {}", m.source_id, m.window.start, m.window.end);
            println!("accounts {}  regions {}", m.accounts.join(","), m.regions.join(","));
            println!("records {}  files {}", opened.records.len(), m.files.len());
            println!(
                "key fingerprint {}  config sha256 {}",
                m.key_fingerprint,
                &m.config_sha256[..16]
            );
            for line in &m.feature_impact {
                println!("rule cost: {line}");
            }
            for line in &m.failures {
                println!("stopped early: {line}");
            }
        }
        Command::Deliver { bundles, to } => {
            let client = client(&cli.config)?;
            let deliverer = client.deliverer();
            if deliver::targets(&client.config).is_empty() {
                bail!("no delivery target is configured: see [deliver] in the config");
            }
            let bundles = if bundles.is_empty() {
                deliverer.pending()?
            } else {
                bundles
            };
            let only = (!to.is_empty()).then_some(to.as_slice());
            let mut failed = 0;
            for b in &bundles {
                if let Err(e) = deliverer.deliver(b, only) {
                    eprintln!("{e:#}");
                    failed += 1;
                }
            }
            if failed > 0 {
                bail!("{failed} of {} bundles were not delivered", bundles.len());
            }
        }
        #[cfg(feature = "upload")]
        Command::Upload { bundles, endpoint } => {
            let loaded = load(&cli.config).ok().map(|(c, _)| c);
            let encrypted = loaded.as_ref().is_some_and(|c| c.encrypt.is_some());
            let configured = loaded.and_then(|c| c.deliver.qloudpulse);
            let mut target = configured.unwrap_or(qloudpulse_collect::config::QloudpulseTarget {
                endpoint: qloudpulse_collect::config::QLOUDPULSE_ENDPOINT.into(),
            });
            if let Some(e) = endpoint {
                target.endpoint = e;
            }
            let transport = UreqTransport::new();
            let token = deliver::qloudpulse::token(&target.endpoint)?;
            for b in &bundles {
                bundle::open(b).with_context(|| format!("{} is not a valid bundle; nothing was sent", b.display()))?;
                // With [encrypt], the JWE next to the bundle is sent, never the plaintext.
                let file = if encrypted {
                    qloudpulse_collect::encrypt::jwe_path(b)
                } else {
                    b.clone()
                };
                let name = file
                    .file_name()
                    .context("a bundle path ends in a file name")?
                    .to_string_lossy()
                    .into_owned();
                let bytes = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
                let detail = deliver::qloudpulse::upload_with(&transport, &target, &token, &name, &bytes)?;
                println!("{detail}");
            }
        }
        #[cfg(feature = "upload")]
        Command::Login { endpoint } => {
            deliver::qloudpulse::sign_in(&endpoint, &mut qloudpulse_stack::token::Tty)?;
            if std::env::var(deliver::qloudpulse::TOKEN_ENV).is_ok_and(|t| !t.trim().is_empty()) {
                eprintln!(
                    "qloudpulse-collect: QLOUDPULSE_TOKEN is set in this shell, and wins over the kept token while it is."
                );
            }
        }
        #[cfg(feature = "upload")]
        Command::Logout { endpoint } => {
            if deliver::qloudpulse::sign_out(&endpoint)? {
                eprintln!(
                    "qloudpulse-collect: signed out on this machine; the token still works until it expires or you remove it."
                );
            } else {
                eprintln!("qloudpulse-collect: no token was kept for {endpoint} on this machine.");
            }
        }
        Command::Run {
            once,
            every,
            ui: true,
            ui_bind,
        } => {
            anyhow::ensure!(!once, "--ui runs the loop; leave out --once");
            if let Some(n) = every {
                anyhow::ensure!(n >= 1, "--every must be at least 1");
            }
            qloudpulse_collect::ui::run_with_ui(cli.config, every, ui_bind)?;
        }
        Command::Ui { ui_bind } => {
            load(&cli.config)?;
            qloudpulse_collect::ui::serve(cli.config, ui_bind)?;
        }
        Command::Run { once, every, .. } => {
            let mut client = client(&cli.config)?;
            if let Some(n) = every {
                anyhow::ensure!(n >= 1, "--every must be at least 1");
                client.config.run.every_hours = n;
            }
            client.pseudonyms().context("the pseudonym key")?;
            if once {
                let path = client.cycle(jiff::Timestamp::now())?;
                println!("{}", path.display());
            } else {
                client.run_forever();
            }
        }
        Command::RotateKey { force } => {
            let (config, _) = load(&cli.config)?;
            let rotated = qloudpulse_collect::pseudonym::rotate(
                &config.key_file,
                &config.work_dir.join("pseudonyms.tsv"),
                force,
            )?;
            println!(
                "key {} replaced by {}",
                rotated.old_fingerprint, rotated.new_fingerprint
            );
            println!("the old key is at {}; back up the new one", rotated.retired.display());
            match rotated.relink {
                Some((path, n)) => println!(
                    "{n} pseudonyms relinked in {}: upload it to qloudpulse to join history",
                    path.display()
                ),
                None => println!("no relink file: history before today will not join what comes after"),
            }
        }
        Command::Policy => {
            let config = load(&cli.config).ok().map(|(c, _)| c);
            let plan = CallPlan::embedded();
            let options = config.as_ref().map(Config::options).unwrap_or_default();
            let mut statements = vec![serde_json::json!({
                "Sid": "QloudpulseCollectRead",
                "Effect": "Allow",
                "Action": plan.iam_actions(&options),
                "Resource": "*",
            })];
            // Cloud Control reads a resource with the underlying service's own read actions, as the caller.
            let mut cc: Vec<&str> = qloudpulse_collect::collect::infra::CLOUD_CONTROL_ACTIONS.to_vec();
            cc.sort();
            cc.dedup();
            statements.push(serde_json::json!({
                "Sid": "QloudpulseCollectCloudControlReads",
                "Effect": "Allow",
                "Action": cc,
                "Resource": "*",
            }));
            if let Some(cur) = config.as_ref().and_then(|c| c.cur.as_ref()) {
                // Athena reads the table's definition with the caller's rights. Read on the CUR export bucket and
                // write on the workgroup's results location are the bucket owner's to grant; see the README.
                statements.push(serde_json::json!({
                    "Sid": "QloudpulseCollectBill",
                    "Effect": "Allow",
                    "Action": ["glue:GetDatabase", "glue:GetTable"],
                    "Resource": [
                        format!("arn:aws:glue:{}:{}:catalog", cur.region, cur.account),
                        format!("arn:aws:glue:{}:{}:database/{}", cur.region, cur.account, cur.database),
                        format!("arn:aws:glue:{}:{}:table/{}/{}", cur.region, cur.account, cur.database, cur.table),
                    ],
                }));
            }
            if let Some(s3) = config.as_ref().and_then(|c| c.deliver.s3.as_ref()) {
                statements.push(serde_json::json!({
                    "Sid": "QloudpulseCollectDeliver",
                    "Effect": "Allow",
                    "Action": ["s3:PutObject"],
                    "Resource": format!("arn:aws:s3:::{}/{}*", s3.bucket, s3.prefix),
                }));
            }
            let policy = serde_json::json!({"Version": "2012-10-17", "Statement": statements});
            println!("{}", serde_json::to_string_pretty(&policy)?);
        }
        Command::CallPlan => {
            println!("{}", serde_json::to_string_pretty(CallPlan::embedded())?);
        }
    }
    Ok(())
}
