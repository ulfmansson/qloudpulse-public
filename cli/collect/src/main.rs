use anyhow::{Context, Result, bail};
use clap::{CommandFactory as _, FromArgMatches as _, Parser, Subcommand};
use qloudpulse_collect::aws::UreqTransport;
use qloudpulse_collect::aws::credentials::CredentialSource;
use qloudpulse_collect::callplan::CallPlan;
use qloudpulse_collect::config::{Config, starter};
use qloudpulse_collect::pseudonym::Pseudonymizer;
use qloudpulse_collect::run::Client;
use qloudpulse_collect::snapshot::Snapshot;
use qloudpulse_collect::view_token::ViewToken;
use qloudpulse_collect::{analyze, bundle, deliver, log, paths};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Collects cloud cost data with your own credentials into a bundle you inspect, redact and deliver.
///
/// Nothing is read from qloudpulse. The only time the client contacts qloudpulse is `upload`, or `run` with
/// [deliver.qloudpulse] configured, and then it sends a finished bundle and nothing else.
#[derive(Parser)]
#[command(version = env!("QLOUDPULSE_LONG_VERSION"), name = "qloudpulse-collect")]
struct Cli {
    /// The config file [default: ./qloudpulse-collect.toml if it exists, else
    /// $XDG_CONFIG_HOME/qloudpulse/qloudpulse-collect.toml or ~/.config/qloudpulse/qloudpulse-collect.toml,
    /// %APPDATA%\qloudpulse\qloudpulse-collect.toml on Windows]
    #[arg(long, global = true, env = "QLOUDPULSE_COLLECT_CONFIG")]
    config: Option<PathBuf>,
    /// Log one JSON object per line, for unattended runs.
    #[arg(long, global = true)]
    log_json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a starter config and create the pseudonym key and the view token.
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
    ///
    /// With no bundle named, it sends the newest: the newest raw snapshot's bundle, packed first if it is not yet
    /// (its raw snapshot is then deleted unless [run] keep_raw), or the newest bundle in output_dir when that is newer.
    ///
    /// A bundle in the config's output_dir that every [deliver] target then has is deleted ([run] delete_delivered).
    #[cfg(feature = "upload")]
    Upload {
        /// The bundles to send; the newest when none is named.
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
        /// Hours between cycles, as `1` or `1h`. Overrides [run] every_hours.
        #[arg(long, value_name = "HOURS", value_parser = hours)]
        every: Option<u32>,
        /// Also serve the local UI, with this loop's cycles in its run history.
        #[arg(long, conflicts_with = "once")]
        ui: bool,
        /// Where the UI listens. Anything but a loopback address lets other machines reach it (the token is still
        /// required).
        #[arg(long, default_value = qloudpulse_collect::ui::DEFAULT_BIND, requires = "ui")]
        ui_bind: std::net::SocketAddr,
        /// A host name the UI also answers to, such as an Ingress's or a Service's (repeatable). Put TLS in front.
        #[arg(long = "ui-allow-host", value_name = "NAME", requires = "ui")]
        ui_allow_host: Vec<String>,
    },
    /// The local web UI: what the client reads, the rules editor with a raw/redacted preview, every bundle, every
    /// run. Listens on 127.0.0.1 and prints an address with a session token.
    Ui {
        /// Where the UI listens. Anything but a loopback address lets other machines reach it (the token is still
        /// required).
        #[arg(long, default_value = qloudpulse_collect::ui::DEFAULT_BIND)]
        ui_bind: std::net::SocketAddr,
        /// A host name the UI also answers to, such as an Ingress's or a Service's (repeatable). Put TLS in front.
        #[arg(long = "ui-allow-host", value_name = "NAME")]
        ui_allow_host: Vec<String>,
    },
    /// Replace the pseudonym key. Writes an old-to-new pseudonym file (no real values) to relink history in
    /// qloudpulse, from the local map (keep_pseudonym_map); without one it refuses unless --force.
    RotateKey {
        #[arg(long)]
        force: bool,
    },
    /// Print the view token (qp_…) that reads `encrypt` rules' values. Enter it in qloudpulse to see them in your
    /// browser; qloudpulse never receives it. It is printed on stdout only, so it can go straight into a password
    /// manager.
    ViewToken {
        /// Create the token file first (a config from before `encrypt` existed has none). Refuses to replace one.
        #[arg(long)]
        create: bool,
        /// Print only the key id that names the token, as bundles and qloudpulse show it.
        #[arg(long, conflicts_with = "create")]
        key_id: bool,
    },
    /// Print the read-only IAM policy the call plan needs (plus S3 delivery, if configured).
    Policy,
    /// Print the compiled-in call plan: every operation the client may call.
    CallPlan,
    /// Install the newest release over this binary, with the release's own install script, checked against the
    /// release's SHA256SUMS. Asks GitHub, and nothing else. Not in a build without the upload: build the new tag the
    /// same way instead.
    #[cfg(feature = "upload")]
    Update {
        /// Say whether a newer release is out, and install nothing.
        #[arg(long, conflicts_with_all = ["version", "attest"])]
        check: bool,
        /// Install this release, newer or older, instead of the newest, such as 0.2.0.
        #[arg(long)]
        version: Option<String>,
        /// Also check the archive's signed build provenance with gh (gh attestation verify); needs gh signed in.
        #[arg(long)]
        attest: bool,
    },
}

/// This client, for `update` and the check for a newer release.
#[cfg(feature = "upload")]
const CLIENT: qloudpulse_stack::update::Client = qloudpulse_stack::update::Client {
    name: "qloudpulse-collect",
    version: env!("CARGO_PKG_VERSION"),
    readme: "cli/collect/README.md",
};

/// Whether this run looks for a newer release on GitHub (at most once a day, see `update::daily_notice`): only the
/// commands a person runs, at a terminal. Never `collect`, `pack`, `deliver`, `upload` or `run`, which are what
/// unattended jobs run, and never in a build without the upload, which contacts nothing but AWS. `ui` checks on its
/// own, for its banner.
#[cfg(feature = "upload")]
fn checks_for_update(cli: &Cli) -> bool {
    use std::io::IsTerminal as _;
    let asked_by_a_person = match cli.command {
        Command::Init
        | Command::Analyze { .. }
        | Command::Verify { .. }
        | Command::Policy
        | Command::CallPlan
        | Command::RotateKey { .. }
        | Command::Login { .. }
        | Command::Logout { .. } => true,
        Command::Collect { .. }
        | Command::Pack { .. }
        | Command::Deliver { .. }
        | Command::Upload { .. }
        | Command::Run { .. }
        | Command::Ui { .. }
        | Command::ViewToken { .. }
        | Command::Update { .. } => false,
    };
    asked_by_a_person && !cli.log_json && std::io::stderr().is_terminal()
}

/// The config file this run would use, read from the arguments before clap has them: for `--help`.
fn config_in_args() -> PathBuf {
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if arg == "--config" {
            if let Some(path) = args.next() {
                return path.into();
            }
        } else if let Some(path) = arg.to_str().and_then(|a| a.strip_prefix("--config=")) {
            return path.into();
        }
    }
    std::env::var_os("QLOUDPULSE_COLLECT_CONFIG")
        .filter(|v| !v.is_empty())
        .map_or_else(paths::default_config, PathBuf::from)
}

/// Where the client keeps its files with the config it would use, worked out now: shown under `--help`.
fn files_help(config_path: &Path) -> String {
    let mut out = String::from("Files on this machine, with this config:\n");
    let mut line = |what: &str, value: String| {
        let _ = writeln!(out, "  {what:<16}{value}");
    };
    let config = if config_path.is_file() {
        line("Config", config_path.display().to_string());
        Config::load(config_path).map(|(c, _)| c)
    } else {
        line(
            "Config",
            format!("{} (not there yet: `init` writes it)", config_path.display()),
        );
        Config::parse_at(starter().as_bytes(), config_path)
    };
    match config {
        Ok(c) => {
            line("Pseudonym key", c.key_file.display().to_string());
            line("View token", c.view_token_file.display().to_string());
            line("Raw snapshots", c.work_dir.join("raw").display().to_string());
            let kept = if c.run.delete_delivered {
                "deleted once every [deliver] target has them"
            } else {
                "kept until keep_bundles pushes them out"
            };
            line("Bundles", format!("{} ({kept})", c.output_dir.display()));
        }
        Err(e) => line("", format!("the rest is unknown: the config does not load ({e:#})")),
    }
    #[cfg(feature = "upload")]
    line("Sign-in token", qloudpulse_stack::credentials::where_kept());
    #[cfg(feature = "upload")]
    if let Some(file) = qloudpulse_stack::update::cache_file(&CLIENT) {
        line("Update check", file.display().to_string());
    }
    out.trim_end().to_string()
}

fn main() {
    let wants_help = std::env::args_os()
        .skip(1)
        .any(|a| a == "-h" || a == "--help" || a == "help");
    let mut command = Cli::command();
    if wants_help {
        command = command.after_help(files_help(&config_in_args()));
    }
    let cli = Cli::from_arg_matches(&command.get_matches()).unwrap_or_else(|e| e.exit());
    log::use_json(cli.log_json);
    // Asked beside the command, so the command does not wait for GitHub; said after it.
    #[cfg(feature = "upload")]
    let newer = checks_for_update(&cli).then(|| std::thread::spawn(|| qloudpulse_stack::update::daily_notice(&CLIENT)));
    let result = run(cli);
    #[cfg(feature = "upload")]
    if let Some(Ok(Some(line))) = newer.map(std::thread::JoinHandle::join) {
        eprintln!("qloudpulse-collect: {line}");
    }
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// `--every`: whole hours, with or without an `h` (`1`, `1h`), so the form the docs and the UI show works as typed.
fn hours(s: &str) -> std::result::Result<u32, String> {
    let n: u32 = s
        .strip_suffix(['h', 'H'])
        .unwrap_or(s)
        .parse()
        .map_err(|_| format!("expected whole hours, such as 1 or 1h, not `{s}`"))?;
    if n == 0 {
        return Err("must be at least 1 hour".into());
    }
    Ok(n)
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

/// The bundle `upload` sends when none is named, packing the newest raw snapshot first if it has none yet.
#[cfg(feature = "upload")]
fn newest_to_upload(config_path: &Path) -> Result<PathBuf> {
    let client = client(config_path)?;
    let config = &client.config;
    let snapshot = match newest_snapshot(config) {
        Ok(dir) => Some(Snapshot::read(&dir)?),
        Err(_) => None,
    };
    match deliver::newest_for_upload(config, snapshot.as_ref().map(|s| &s.meta.window))? {
        Some(deliver::Newest::Bundle(path)) => {
            eprintln!("qloudpulse-collect: uploading the newest bundle, {}", path.display());
            Ok(path)
        }
        Some(deliver::Newest::Pack) => {
            let snapshot = snapshot.expect("Pack only for a snapshot");
            eprintln!(
                "qloudpulse-collect: packing the newest raw snapshot, {}, to upload it",
                snapshot.dir.display()
            );
            let path = client.pack(&snapshot, false)?;
            if !config.run.keep_raw {
                std::fs::remove_dir_all(&snapshot.dir)
                    .with_context(|| format!("removing {}", snapshot.dir.display()))?;
            }
            Ok(path)
        }
        None => bail!(
            "nothing to upload: no bundle in {} and no raw snapshot in {}; run `qloudpulse-collect collect` first, \
             or name a bundle",
            config.output_dir.display(),
            config.work_dir.join("raw").display()
        ),
    }
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
    let config_path = cli.config.clone().unwrap_or_else(paths::default_config);
    match cli.command {
        Command::Init => {
            if config_path.exists() {
                println!("{} exists; left as it is", config_path.display());
            } else {
                paths::create_parent(&config_path)
                    .with_context(|| format!("creating the directory for {}", config_path.display()))?;
                std::fs::write(&config_path, starter())?;
                println!("wrote {}", config_path.display());
            }
            let (config, _) = load(&config_path)?;
            if config.key_file.exists() {
                println!("{} exists; left as it is", config.key_file.display());
            } else {
                Pseudonymizer::create_key(&config.key_file)?;
                println!(
                    "created the pseudonym key {}. Back it up: losing it changes every pseudonym.",
                    config.key_file.display()
                );
            }
            if config.view_token_file.exists() {
                println!("{} exists; left as it is", config.view_token_file.display());
            } else {
                let token = ViewToken::create(&config.view_token_file)?;
                println!(
                    "created the view token {} (key id {}). Back it up: without it nobody can read encrypted values. \
                     `view-token` prints it.",
                    config.view_token_file.display(),
                    token.key_id()
                );
            }
        }
        Command::Collect { daily } => {
            let mut client = client(&config_path)?;
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
            let config = load(&config_path).ok().map(|(c, _)| c);
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
            let client = client(&config_path)?;
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
            let client = client(&config_path)?;
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
            let loaded = load(&config_path).ok().map(|(c, _)| c);
            let encrypted = loaded.as_ref().is_some_and(|c| c.encrypt.is_some());
            let configured = loaded.as_ref().and_then(|c| c.deliver.qloudpulse.clone());
            let mut target = configured.unwrap_or(qloudpulse_collect::config::QloudpulseTarget {
                endpoint: qloudpulse_collect::config::QLOUDPULSE_ENDPOINT.into(),
            });
            if let Some(e) = endpoint {
                target.endpoint = e;
            }
            let transport = UreqTransport::new();
            // Chosen (and packed) before signing in, so "nothing to upload" comes without a sign-in first.
            let bundles = if bundles.is_empty() {
                vec![newest_to_upload(&config_path)?]
            } else {
                bundles
            };
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
                // A bundle of the config's own output_dir that every target now has is deleted ([run] delete_delivered).
                if let Some(config) = &loaded {
                    let deliverer = deliver::Deliverer {
                        config,
                        transport: &transport,
                        credentials: None,
                    };
                    deliverer.record_delivered(b, "qloudpulse")?;
                }
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
            ui_allow_host,
        } => {
            anyhow::ensure!(!once, "--ui runs the loop; leave out --once");
            let options = qloudpulse_collect::ui::Options::new(ui_bind, ui_allow_host)?;
            qloudpulse_collect::ui::run_with_ui(config_path, every, options)?;
        }
        Command::Ui { ui_bind, ui_allow_host } => {
            load(&config_path)?;
            #[cfg(feature = "upload")]
            let newer = (!cli.log_json).then_some(|| qloudpulse_stack::update::daily_notice(&CLIENT));
            #[cfg(not(feature = "upload"))]
            let newer: Option<fn() -> Option<String>> = None;
            let options = qloudpulse_collect::ui::Options::new(ui_bind, ui_allow_host)?;
            qloudpulse_collect::ui::serve(config_path, options, newer)?;
        }
        Command::Run { once, every, .. } => {
            let mut client = client(&config_path)?;
            if let Some(n) = every {
                client.config.run.every_hours = n;
            }
            client.pseudonyms().context("the pseudonym key or the view token")?;
            if once {
                let path = client.cycle(jiff::Timestamp::now())?;
                println!("{}", path.display());
            } else {
                client.run_forever();
            }
        }
        Command::RotateKey { force } => {
            let (config, _) = load(&config_path)?;
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
        Command::ViewToken { create, key_id } => {
            let (config, _) = load(&config_path)?;
            let path = &config.view_token_file;
            let token = if create {
                let token = ViewToken::create(path)?;
                eprintln!(
                    "created the view token {}. Back it up: without it nobody can read encrypted values.",
                    path.display()
                );
                token
            } else {
                ViewToken::load(path)?
            };
            if key_id {
                println!("{}", token.key_id());
            } else {
                eprintln!(
                    "key id {}. Enter the token in qloudpulse where it asks for one; your browser decrypts the values \
                     and qloudpulse never receives the token. Anyone holding it reads every value it encrypted.",
                    token.key_id()
                );
                println!("{}", token.text());
            }
        }
        Command::Policy => {
            let config = load(&config_path).ok().map(|(c, _)| c);
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
        #[cfg(feature = "upload")]
        Command::Update { check, version, attest } => {
            CLIENT.update(&qloudpulse_stack::update::Options { check, version, attest })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_takes_hours_with_or_without_the_unit() {
        assert_eq!(hours("1"), Ok(1));
        assert_eq!(hours("1h"), Ok(1));
        assert_eq!(hours("24H"), Ok(24));
        assert!(hours("0").is_err());
        assert!(hours("0h").is_err());
        assert!(hours("90m").is_err());
        assert!(hours("h").is_err());
    }

    #[test]
    fn the_command_the_ui_suggests_parses() {
        let cli = Cli::try_parse_from(["qloudpulse-collect", "run", "--every", "1h", "--ui"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Run {
                every: Some(1),
                ui: true,
                ..
            }
        ));
    }
}
