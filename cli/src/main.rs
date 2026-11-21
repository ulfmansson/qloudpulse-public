use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{IsTerminal as _, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Command as Process, ExitCode, ExitStatus, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};

use qloudpulse_cli::catalogue::Catalogue;
use qloudpulse_cli::credentials::{self, Kept, Saved};
use qloudpulse_cli::login::{self, SignedIn};
use qloudpulse_cli::pulumi::{self, Options};
use qloudpulse_cli::render::{self, Style};
use qloudpulse_cli::request::{ChangeKind, FORMAT_VERSION, MAX_RESOURCES, Response};
use qloudpulse_cli::{send, token, version, wrap};

#[derive(Parser)]
#[command(name = "qloudpulse", version = version::LONG_VERSION, about = "qloudpulse from the command line", after_help = CONNECT)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// How the client reaches a company, shown under every help text.
const CONNECT: &str = "\
Connecting to your company:
  Anyone in the company makes their own token under Your tokens (under your name); an admin's
  event-source token from Settings, Events works too. On hosted qloudpulse, Your tokens is
  https://app.qloudpulse.com/ui/?profile#my-tokens.
  Put the token in QLOUDPULSE_TOKEN, for example from a CI secret, never in an option, so it
  stays out of logs and shell history. On your own machine, `qloudpulse login` signs in with
  your browser and keeps a token in the system keychain; QLOUDPULSE_TOKEN wins over it. With
  neither, a run at a terminal offers to sign in, or takes a pasted token, hidden, for that
  run only.

  Prices are your company's own when its AWS bill is connected in qloudpulse (/account/aws);
  otherwise they are list prices.";

/// The environment the commands that price read, shown under their help.
const ENVIRONMENT: &str = "\
Environment:
  QLOUDPULSE_TOKEN      A token of your own (qpev_...), from Your tokens under your name in
                        qloudpulse. Needed on every run but --dry-run, even one with nothing
                        to price. Unset, the token `qloudpulse login` kept is used; with none,
                        a run at a terminal offers to sign in with the browser; in CI or
                        without a terminal the run stops
  QLOUDPULSE_ENDPOINT   The same as --endpoint
  QLOUDPULSE_CATALOGUE  The same as --catalogue
  QLOUDPULSE_CREDENTIALS_FILE
                        Keep the token of `qloudpulse login` in this file (0600) instead of
                        the system keychain";

/// How `estimate` is used, shown under its help.
const ESTIMATE_EXAMPLE: &str = "\
Example:
  pulumi preview --json --show-sames -s prod | qloudpulse estimate > comment.md";

/// Where Pulumi's arguments start, shown under `pulumi preview` and `pulumi up`.
const PULUMI_ARGS: &str = "\
Pulumi's arguments:
  qloudpulse's options come first. From the first word it does not know, everything goes to
  Pulumi as it is, so `qloudpulse pulumi preview -s prod --format json` hands --format to
  Pulumi. A -- in front of Pulumi's arguments makes the split plain. Pulumi's --json is
  refused, as the estimate is printed on the same stdout; for JSON, run
  `pulumi preview --json --show-sames | qloudpulse estimate`. Pulumi's flags are read as words of
  their own: -y, not -yf.

  The priced preview has no terminal to prompt on: a stack with a passphrase needs
  PULUMI_CONFIG_PASSPHRASE or PULUMI_CONFIG_PASSPHRASE_FILE.";

/// How `pulumi preview` runs, shown under its help.
const PULUMI_PREVIEW: &str = "\
How it runs:
  `pulumi preview` runs with your arguments and shows its usual diff, then
  `pulumi preview --json --show-sames` runs with the same arguments and is priced: Pulumi prints
  its diff or its JSON, never both, so the stack is previewed twice. The estimate follows the
  diff on stdout. A preview that fails ends the run with Pulumi's exit code. An estimate that
  fails (no token, the network, a refused version) exits 1, after the diff.

Examples:
  qloudpulse pulumi preview -s payments-prod
  qloudpulse pulumi preview --format json -- -s payments-prod --diff";

/// How `pulumi up` runs, shown under its help.
const PULUMI_UP: &str = "\
How it runs:
  First `pulumi preview --json --show-sames` runs with your arguments, less those only
  `pulumi up` takes (-y/--yes, -f/--skip-preview, --continue-on-error, --strict, --plan,
  --secrets-provider, --expect-no-changes), and the estimate is printed. Then `pulumi up` runs
  with your arguments as they are, on your terminal: its own preview and diff, and its prompt
  \"Do you want to perform this update?\", so the stack is previewed twice. The run ends with Pulumi's
  exit code.

  With -y/--yes the estimate is printed and the update goes ahead without a pause: it is in the
  log, not a question. With -f/--skip-preview nothing is priced.

  An estimate that fails (no token, the network, a refused version) does not stop the update:
  one line on stderr says why, and pulumi up runs. With --require-estimate it stops there
  instead, and pulumi up does not run.

Examples:
  qloudpulse pulumi up -s payments-prod
  qloudpulse pulumi up --require-estimate -- -s payments-prod --yes";

#[derive(Subcommand)]
enum Command {
    /// Price an infrastructure change before it is applied, from `pulumi preview --json`.
    ///
    /// Prints a Markdown comment for the pull request. Which resource types and attributes are sent is qloudpulse's:
    /// the client sends it the types the preview holds (no token needed) and sends what it answers, or reads
    /// --catalogue.
    #[command(after_help = format!("{ENVIRONMENT}\n\n{ESTIMATE_EXAMPLE}\n\n{CONNECT}"))]
    Estimate(EstimateArgs),

    /// Run `pulumi preview` or `pulumi up` with your own arguments, and price the change on the way.
    #[command(subcommand, after_help = CONNECT)]
    Pulumi(PulumiCommand),

    /// Sign in with your browser, and keep a token on this machine for the runs after.
    ///
    /// Shows a one-time code and opens qloudpulse's page in your browser, where you check the code and approve it.
    /// qloudpulse then makes a token of your own, listed under Your tokens, and the client keeps it in the system
    /// keychain. Nothing listens on a port, so it works over SSH: open the address on any device.
    #[command(after_help = format!("{LOGIN}\n\n{CONNECT}"))]
    Login(AccountArgs),

    /// Remove the token `qloudpulse login` kept on this machine.
    ///
    /// The token still works until it expires or you remove it under Your tokens in qloudpulse.
    #[command(after_help = CONNECT)]
    Logout(AccountArgs),
}

/// How `login` keeps its token, shown under its help.
const LOGIN: &str = "\
Where the token is kept:
  In the macOS Keychain, the Windows Credential Manager or Linux's Secret Service, one for each
  qloudpulse address. Without a keychain, as on a headless Linux machine, in a file only you can
  read: ~/.config/qloudpulse/credentials (%APPDATA%\\qloudpulse\\credentials on Windows), or the
  file QLOUDPULSE_CREDENTIALS_FILE names. QLOUDPULSE_TOKEN, when set, wins over the kept token.
  A kept token past its expiry counts as none.";

/// What `login` and `logout` take.
#[derive(clap::Args)]
struct AccountArgs {
    /// The qloudpulse to sign in to, by the address `estimate` sends to. On hosted qloudpulse the sign-in is at
    /// https://app.qloudpulse.com.
    #[arg(long, env = "QLOUDPULSE_ENDPOINT", default_value = send::DEFAULT_ENDPOINT)]
    endpoint: String,
}

#[derive(Subcommand)]
enum PulumiCommand {
    /// `pulumi preview` with its usual diff, then the estimate of the change.
    ///
    /// Prints Pulumi's diff, then the comment `qloudpulse estimate` prints: text at a terminal, Markdown piped, or with
    /// --format json its answer.
    #[command(after_help = format!("{PULUMI_PREVIEW}\n\n{PULUMI_ARGS}\n\n{ENVIRONMENT}\n\n{CONNECT}"))]
    Preview(PreviewArgs),

    /// The estimate of the change, then `pulumi up` as you would run it.
    ///
    /// Prices the change first, then runs `pulumi up` on your terminal, its diff and its prompt as usual, and ends
    /// with its exit code.
    #[command(after_help = format!("{PULUMI_UP}\n\n{PULUMI_ARGS}\n\n{ENVIRONMENT}\n\n{CONNECT}"))]
    Up(UpArgs),
}

#[derive(clap::Args)]
struct EstimateArgs {
    /// The JSON of `pulumi preview --json`. Without it, or with -, it is read from stdin.
    file: Option<PathBuf>,

    #[command(flatten)]
    price: PriceArgs,
}

/// What every command that prices takes.
#[derive(clap::Args)]
struct PriceArgs {
    /// Print the request that would be sent, and send nothing. The only run that needs no token.
    #[arg(long)]
    dry_run: bool,

    /// Where to send the request. The types are asked about at this address and /catalogue.
    #[arg(long, env = "QLOUDPULSE_ENDPOINT", default_value = send::DEFAULT_ENDPOINT)]
    endpoint: String,

    /// Read the catalogue of priced types from this file instead of asking the endpoint: a saved answer, to pin what is
    /// sent.
    #[arg(long, env = "QLOUDPULSE_CATALOGUE")]
    catalogue: Option<PathBuf>,

    /// The AWS region, such as eu-west-1, of every resource for which neither the preview nor its provider names one.
    ///
    /// A resource's region is, first found: its own region (AWS provider v7), its provider's region, --region, the
    /// stack's aws:region, AWS_REGION, AWS_DEFAULT_REGION, and the region of the AWS profile in use (the provider's
    /// profile, else the stack's aws:profile, else AWS_PROFILE, else default) in AWS_CONFIG_FILE or ~/.aws/config. Only
    /// the region is read from that file, never credentials. --region comes before the stack and this machine, as
    /// the choice made for this run.
    #[arg(long)]
    region: Option<String>,

    /// Send the cloud ID of each resource that exists before the change, so its usage comes from what it really used.
    #[arg(long)]
    with_resource_ids: bool,

    /// Ask qloudpulse to price the change and keep nothing of it: no names or URNs are sent, and there is no page, link
    /// or point in the stack's history. Without it qloudpulse keeps the estimate with its resources' names, unless your
    /// company chose to keep nothing.
    #[arg(long, env = "QLOUDPULSE_NO_STORE")]
    no_store: bool,

    /// What to print: the comment in Markdown, for a pull request, or as plain text, or the answer as JSON. Without
    /// it: text when stdout is a terminal, Markdown when it is piped or redirected (`> comment.md`).
    #[arg(long, value_enum)]
    format: Option<Format>,
}

#[derive(clap::Args)]
struct PreviewArgs {
    #[command(flatten)]
    price: PriceArgs,

    /// The arguments of `pulumi preview`, such as -s payments-prod --diff. They follow qloudpulse's options.
    #[arg(value_name = "PULUMI_ARGS", trailing_var_arg = true, allow_hyphen_values = true)]
    pulumi_args: Vec<OsString>,
}

#[derive(clap::Args)]
struct UpArgs {
    #[command(flatten)]
    price: PriceArgs,

    /// Stop, and do not run pulumi up, when the change cannot be priced. Without it a failed estimate is a line on
    /// stderr, and the update goes on.
    #[arg(long)]
    require_estimate: bool,

    /// The arguments of `pulumi up`, such as -s payments-prod --yes. They follow qloudpulse's options.
    #[arg(value_name = "PULUMI_ARGS", trailing_var_arg = true, allow_hyphen_values = true)]
    pulumi_args: Vec<OsString>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Markdown,
    Text,
    Json,
}

/// The format a run prints: the one asked for, else text for a person at a terminal and Markdown for a file or a pipe,
/// so CI's `> comment.md` keeps Markdown.
fn format(asked: Option<Format>, stdout_is_terminal: bool) -> Format {
    asked.unwrap_or(if stdout_is_terminal {
        Format::Text
    } else {
        Format::Markdown
    })
}

fn read_input(file: Option<&PathBuf>) -> Result<String> {
    let mut input = String::new();
    match file {
        Some(path) if path.as_os_str() != "-" => {
            input = std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
        }
        _ => {
            std::io::stdin()
                .read_to_string(&mut input)
                .context("could not read stdin")?;
        }
    }
    Ok(input)
}

/// What is about to be priced, for stderr: the change's resources, and the stack's unchanged ones.
fn counts(estimate: &pulumi::Estimate) -> String {
    let request = &estimate.request;
    let same = request
        .resources
        .iter()
        .filter(|r| r.change == ChangeKind::Same)
        .count();
    let not_priced: u32 = request.not_priced.iter().map(|n| n.count).sum();
    let mut line = format!(
        "{} resources to price, {not_priced} not priced",
        request.resources.len() - same
    );
    if let Some(n) = estimate.stack_too_large {
        let _ = write!(
            line,
            "; the stack's cost is left out, as its {n} resources are more than one estimate takes ({MAX_RESOURCES})"
        );
        if same > 0 {
            let _ = write!(line, ", and {same} unchanged that they refer to go");
        }
    } else if estimate.stack_listed {
        let _ = write!(
            line,
            "; and {same} unchanged, for the stack's cost, {} not priced",
            estimate.stack_not_priced
        );
    }
    line
}

/// The token a run that sends needs, checked before anything else: from QLOUDPULSE_TOKEN, else the one kept by
/// `qloudpulse login`, else, at a terminal, from signing in with the browser or a pasted one. None for a dry run.
fn token_for(args: &PriceArgs) -> Result<Option<String>> {
    if args.dry_run {
        return Ok(None);
    }
    let endpoint = &args.endpoint;
    token::resolve(
        std::env::var("QLOUDPULSE_TOKEN").ok(),
        endpoint,
        || kept_token(endpoint),
        &mut token::Tty,
        |terminal| Ok(sign_in(endpoint, terminal)?.token),
    )
    .map(Some)
}

/// The token `qloudpulse login` kept for the qloudpulse behind the endpoint, unless it has expired.
fn kept_token(endpoint: &str) -> Option<String> {
    let origin = login::origin(endpoint).ok()?;
    credentials::system()
        .load(&origin, credentials::now())
        .map(|saved| saved.token)
}

/// Signs in with the browser at the qloudpulse behind the endpoint, keeps the token, and says where.
fn sign_in(endpoint: &str, terminal: &mut impl token::Terminal) -> Result<SignedIn> {
    let origin = login::origin(endpoint)?;
    let signed_in = login::sign_in(
        &origin,
        &mut login::Network,
        &mut login::SystemClock::default(),
        &mut login::SystemBrowser,
        terminal,
    )?;
    let saved = Saved {
        token: signed_in.token.clone(),
        company: signed_in.company.clone(),
        expires_at: signed_in.expires_at.clone(),
    };
    let company = login::plain(&signed_in.company);
    let lasts = match signed_in.expires_at.as_deref().and_then(credentials::day) {
        Some(day) => format!("The token lasts until {day}"),
        None => "The token is yours".to_string(),
    };
    let page = login::tokens_page(&origin);
    match credentials::system().save(&origin, &saved) {
        Ok(kept) => {
            terminal.say(&format!(
                "qloudpulse: signed in to {company}. {lasts}; remove it under Your tokens ({page})."
            ));
            terminal.say(&match kept {
                Kept::Keychain => "qloudpulse: it is kept in the system keychain for the runs after this one.".into(),
                Kept::File(path) => format!(
                    "qloudpulse: there is no system keychain here, so it is kept in {}, which only you can read.",
                    path.display()
                ),
            });
        }
        // The token works for this run even when it cannot be kept.
        Err(e) => terminal.say(&format!(
            "qloudpulse: signed in to {company}, for this run only: {e:#}. {lasts}; remove it under Your tokens \
             ({page})."
        )),
    }
    Ok(signed_in)
}

fn login_command(args: &AccountArgs) -> Result<ExitCode> {
    let mut terminal = token::Tty;
    sign_in(&args.endpoint, &mut terminal)?;
    if std::env::var("QLOUDPULSE_TOKEN").is_ok_and(|t| !t.trim().is_empty()) {
        eprintln!("qloudpulse: QLOUDPULSE_TOKEN is set in this shell, and wins over the kept token while it is.");
    }
    Ok(ExitCode::SUCCESS)
}

fn logout_command(args: &AccountArgs) -> Result<ExitCode> {
    let origin = login::origin(&args.endpoint)?;
    let page = login::tokens_page(&origin);
    if credentials::system().remove(&origin)? {
        eprintln!(
            "qloudpulse: signed out of {origin} on this machine. The token still exists in qloudpulse until it \
             expires or you remove it under Your tokens: {page}"
        );
    } else {
        eprintln!("qloudpulse: no token was kept for {origin} on this machine.");
    }
    if std::env::var("QLOUDPULSE_TOKEN").is_ok_and(|t| !t.trim().is_empty()) {
        eprintln!("qloudpulse: QLOUDPULSE_TOKEN is still set in this shell, and runs use it.");
    }
    Ok(ExitCode::SUCCESS)
}

fn check_region(args: &PriceArgs) -> Result<()> {
    if let Some(region) = &args.region
        && !pulumi::is_region(region)
    {
        bail!("--region takes an AWS region name, such as eu-west-1");
    }
    Ok(())
}

/// Prices the JSON of `pulumi preview --json` and returns what goes to stdout: the comment, the answer as JSON, or with
/// --dry-run the request. What is about to be priced, and a newer release, are said on stderr.
fn price(args: &PriceArgs, token: Option<&str>, input: &str) -> Result<String> {
    let preview = pulumi::parse(input)?;
    let options = Options {
        region: args.region.clone(),
        aws: pulumi::AwsEnvironment::from_system(),
        with_resource_ids: args.with_resource_ids,
        keep_nothing: args.no_store,
    };
    let catalogue = match &args.catalogue {
        Some(path) => Catalogue::parse(
            &std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?,
        )?,
        None => send::fetch_catalogue(&args.endpoint, &pulumi::found_types(&preview))?,
    };
    // Below the oldest client qloudpulse takes, stop before anything is sent; a newer release is said on stderr and
    // under the comment, where a CI log nobody reads cannot hide it.
    let newer = version::check(version::VERSION, catalogue.client.as_ref())?;
    if let Some(line) = &newer {
        eprintln!("qloudpulse: {line}");
    }
    let estimate = pulumi::estimate(&preview, &catalogue, &options);
    let request = &estimate.request;

    eprintln!("qloudpulse: {}", counts(&estimate));
    if estimate.no_region > 0 {
        eprintln!("qloudpulse: {}", render::no_region(estimate.no_region, Style::Text));
    }

    // No token only with --dry-run.
    let Some(token) = token else {
        return Ok(format!("{}\n", serde_json::to_string_pretty(request)?));
    };

    // Nothing to price: no call. Unchanged resources alone are still sent, for the stack's cost.
    let response = if request.resources.is_empty() {
        Response {
            format_version: FORMAT_VERSION,
            estimate_id: None,
            currency: "USD".into(),
            url: None,
            stored: None,
            resources: vec![],
        }
    } else {
        let response = send::send(&args.endpoint, token, request)?;
        if args.no_store {
            check_kept_nothing(&response)?;
        }
        response
    };

    let style = match format(args.format, std::io::stdout().is_terminal()) {
        Format::Json => return Ok(format!("{}\n", serde_json::to_string_pretty(&response)?)),
        Format::Markdown => Style::Markdown,
        Format::Text => Style::Text,
    };
    let mut out = render::render(&response, &estimate, style);
    if let Some(line) = &newer {
        let _ = writeln!(out, "\n{}", style.small(line));
    }
    Ok(out)
}

/// With `--no-store`, that qloudpulse says it kept nothing. An endpoint from before ADR 0140 does not read `store` and
/// keeps the estimate, without names: its answer, with an id or a page or without `stored: false`, is refused rather
/// than printed as if nothing had been kept.
fn check_kept_nothing(response: &Response) -> Result<()> {
    if response.kept_nothing() {
        return Ok(());
    }
    bail!(
        "--no-store, but this qloudpulse kept the estimate: it is older than --no-store and keeps every estimate it \
         prices (without resource names). Leave out --no-store, or ask your qloudpulse's admins to update it"
    )
}

fn estimate(args: &EstimateArgs) -> Result<ExitCode> {
    // Every run but a dry run needs the token, whatever the preview holds: checked first, so a missing one is said
    // before the preview is read or anything reaches the network.
    let token = token_for(&args.price)?;
    check_region(&args.price)?;
    let input = read_input(args.file.as_ref())?;
    print!("{}", price(&args.price, token.as_deref(), &input)?);
    Ok(ExitCode::SUCCESS)
}

/// Where `pulumi` is: the first directory on PATH that holds it.
fn find_pulumi() -> Result<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["pulumi.exe", "pulumi.cmd", "pulumi.bat"]
    } else {
        &["pulumi"]
    };
    std::env::var_os("PATH")
        .iter()
        .flat_map(std::env::split_paths)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            anyhow!(
                "pulumi is not on PATH: install it (https://www.pulumi.com/docs/install/) or add its directory to PATH"
            )
        })
}

/// Runs Pulumi on the terminal, its stdin, stdout and stderr the user's, as if they had run it themselves.
fn run_pulumi(pulumi: &Path, command: &str, args: &[OsString]) -> Result<ExitStatus> {
    Process::new(pulumi)
        .arg(command)
        .args(args)
        .status()
        .with_context(|| format!("could not run {}", pulumi.display()))
}

/// Runs the priced preview, `pulumi preview --json --show-sames` with the user's arguments, and returns its JSON.
/// Pulumi's stderr is the terminal's, so its errors show; it gets no stdin, so it cannot wait on a prompt nobody sees
/// or read what is meant for the `pulumi up` after it.
fn priced_preview(pulumi: &Path, args: &[OsString]) -> Result<String> {
    let out = Process::new(pulumi)
        .args(wrap::priced_preview(args))
        // With it, Pulumi streams engine events instead of the digest the client reads.
        .env_remove("PULUMI_ENABLE_STREAMING_JSON_PREVIEW")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("could not run {}", pulumi.display()))?;
    if !out.status.success() {
        bail!("`pulumi preview --json --show-sames` failed ({})", out.status);
    }
    String::from_utf8(out.stdout).context("`pulumi preview --json` printed what is not UTF-8")
}

/// Pulumi's own exit code, or failure when it has none (killed by a signal) or one past what an exit code holds here.
fn exit_code(status: ExitStatus) -> ExitCode {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return u8::try_from(128 + signal).map_or(ExitCode::FAILURE, ExitCode::from);
    }
    status
        .code()
        .and_then(|c| u8::try_from(c).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from)
}

/// Pulumi's --json would share stdout with the estimate.
fn refuse_json(args: &[OsString], command: &str) -> Result<()> {
    if wrap::wants_json(args) {
        bail!(
            "qloudpulse pulumi {command} takes no --json, as the estimate is printed on the same stdout: run \
             `pulumi preview --json --show-sames | qloudpulse estimate` for JSON, or `pulumi {command} --json` without \
             qloudpulse"
        );
    }
    Ok(())
}

fn pulumi_preview(args: &PreviewArgs) -> Result<ExitCode> {
    refuse_json(&args.pulumi_args, "preview")?;
    check_region(&args.price)?;
    let pulumi = find_pulumi()?;
    let shown = run_pulumi(&pulumi, "preview", &args.pulumi_args)?;
    if !shown.success() {
        return Ok(exit_code(shown));
    }
    let token = token_for(&args.price)?;
    let json = priced_preview(&pulumi, &args.pulumi_args)?;
    let out = price(&args.price, token.as_deref(), &json)?;
    // A line between Pulumi's display and the comment.
    print!("\n{out}");
    Ok(ExitCode::SUCCESS)
}

/// The estimate `pulumi up` prints before it runs.
fn price_up(pulumi: &Path, args: &UpArgs) -> Result<String> {
    let token = token_for(&args.price)?;
    let json = priced_preview(pulumi, &args.pulumi_args)?;
    price(&args.price, token.as_deref(), &json)
}

fn pulumi_up(args: &UpArgs) -> Result<ExitCode> {
    let pulumi_args = &args.pulumi_args;
    refuse_json(pulumi_args, "up")?;
    check_region(&args.price)?;
    let pulumi = find_pulumi()?;
    if wrap::skips_preview(pulumi_args) {
        if args.require_estimate {
            bail!("--require-estimate needs the preview that --skip-preview skips");
        }
        eprintln!("qloudpulse: --skip-preview, so nothing is priced");
    } else {
        match price_up(&pulumi, args) {
            Ok(out) => {
                print!("{out}");
                if wrap::says_yes(pulumi_args) {
                    eprintln!("qloudpulse: --yes, so pulumi up goes ahead without asking");
                }
            }
            Err(e) if args.require_estimate => {
                return Err(e.context("no estimate, so pulumi up was not run (--require-estimate)"));
            }
            Err(e) => eprintln!("qloudpulse: no estimate, pulumi up goes on: {e:#}"),
        }
    }
    Ok(exit_code(run_pulumi(&pulumi, "up", pulumi_args)?))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Estimate(args) => estimate(args),
        Command::Pulumi(PulumiCommand::Preview(args)) => pulumi_preview(args),
        Command::Pulumi(PulumiCommand::Up(args)) => pulumi_up(args),
        Command::Login(args) => login_command(args),
        Command::Logout(args) => logout_command(args),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("qloudpulse: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_format_a_terminal_gets_text_and_a_file_or_pipe_markdown() {
        assert_eq!(format(None, true), Format::Text);
        assert_eq!(format(None, false), Format::Markdown);
        for asked in [Format::Markdown, Format::Text, Format::Json] {
            assert_eq!(format(Some(asked), true), asked);
            assert_eq!(format(Some(asked), false), asked);
        }
    }
}
