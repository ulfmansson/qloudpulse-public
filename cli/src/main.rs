use std::fmt::Write as _;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use qloudpulse_cli::catalogue::Catalogue;
use qloudpulse_cli::pulumi::{self, Options};
use qloudpulse_cli::request::{ChangeKind, FORMAT_VERSION, MAX_RESOURCES, Response};
use qloudpulse_cli::{render, send, version};

#[derive(Parser)]
#[command(name = "qloudpulse", version, about = "qloudpulse from the command line", after_help = CONNECT)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// How the client reaches a company, shown under every help text.
const CONNECT: &str = "\
Connecting to your company:
  An admin in qloudpulse opens Settings, Events, adds a source and copies its token (qpev_...).
  On hosted qloudpulse that is https://app.qloudpulse.com/ui/?events.
  Put the token in QLOUDPULSE_TOKEN, for example from a CI secret. It is read from there only,
  never from an option, so it stays out of logs and shell history.

  Prices are your company's own when its AWS bill is connected in qloudpulse (/account/aws);
  otherwise they are list prices.";

/// The environment `estimate` reads, shown under its help.
const ESTIMATE_ENVIRONMENT: &str = "\
Environment:
  QLOUDPULSE_TOKEN     A change-event source's token (qpev_...), from Settings, Events in
                       qloudpulse. Needed on every run but --dry-run, even one with nothing
                       to price
  QLOUDPULSE_ENDPOINT  The same as --endpoint

Example:
  pulumi preview --json --show-sames -s prod | qloudpulse estimate > comment.md";

#[derive(Subcommand)]
enum Command {
    /// Price an infrastructure change before it is applied, from `pulumi preview --json`.
    ///
    /// Prints a Markdown comment for the pull request. Which resource types and attributes are sent is qloudpulse's:
    /// the client sends it the types the preview holds (no token needed) and sends what it answers, or reads
    /// --catalogue.
    #[command(after_help = format!("{ESTIMATE_ENVIRONMENT}\n\n{CONNECT}"))]
    Estimate(EstimateArgs),
}

#[derive(clap::Args)]
struct EstimateArgs {
    /// The JSON of `pulumi preview --json`. Without it, or with -, it is read from stdin.
    file: Option<PathBuf>,

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

    /// The AWS region of resources whose provider and stack name none, such as eu-west-1.
    #[arg(long)]
    region: Option<String>,

    /// Send the cloud ID of each resource that exists before the change, so its usage comes from what it really used.
    #[arg(long)]
    with_resource_ids: bool,

    /// What to print: the comment, or the answer as JSON.
    #[arg(long, value_enum, default_value_t = Format::Markdown)]
    format: Format,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Markdown,
    Json,
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

/// The token from QLOUDPULSE_TOKEN, trimmed, or the error that says where the endpoint's tokens are made.
fn token(endpoint: &str) -> Result<String> {
    std::env::var("QLOUDPULSE_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .with_context(|| send::missing_token(endpoint))
}

fn estimate(args: &EstimateArgs) -> Result<()> {
    // Every run but a dry run needs the token, whatever the preview holds: checked first, so a missing one is said
    // before the preview is read or anything reaches the network.
    let token = if args.dry_run {
        None
    } else {
        Some(token(&args.endpoint)?)
    };
    if let Some(region) = &args.region
        && !pulumi::is_region(region)
    {
        bail!("--region takes an AWS region name, such as eu-west-1");
    }
    let preview = pulumi::parse(&read_input(args.file.as_ref())?)?;
    let options = Options {
        region: args.region.clone(),
        with_resource_ids: args.with_resource_ids,
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

    // No token only with --dry-run.
    let Some(token) = token else {
        println!("{}", serde_json::to_string_pretty(request)?);
        return Ok(());
    };

    // Nothing to price: no call. Unchanged resources alone are still sent, for the stack's cost.
    let response = if request.resources.is_empty() {
        Response {
            format_version: FORMAT_VERSION,
            estimate_id: None,
            currency: "USD".into(),
            resources: vec![],
        }
    } else {
        send::send(&args.endpoint, &token, request)?
    };

    match args.format {
        Format::Markdown => {
            print!("{}", render::comment(&response, &estimate));
            if let Some(line) = &newer {
                println!("\n<sub>{line}</sub>");
            }
        }
        Format::Json => println!("{}", serde_json::to_string_pretty(&response)?),
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Command::Estimate(args) => estimate(args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("qloudpulse: {e:#}");
            ExitCode::FAILURE
        }
    }
}
