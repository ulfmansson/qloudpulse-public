use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use qloudpulse_cli::catalogue::Catalogue;
use qloudpulse_cli::pulumi::{self, Options};
use qloudpulse_cli::request::{ChangeKind, FORMAT_VERSION, Response};
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
  Put the token in QLOUDPULSE_TOKEN, for example from a CI secret. It is read from there only,
  never from an option, so it stays out of logs and shell history.

  Prices are your company's own when its AWS bill is connected in qloudpulse (/account/aws);
  otherwise they are list prices.";

/// The environment `estimate` reads, shown under its help.
const ESTIMATE_ENVIRONMENT: &str = "\
Environment:
  QLOUDPULSE_TOKEN     A change-event source's token (qpev_...), from Settings, Events in
                       qloudpulse. Needed unless --dry-run
  QLOUDPULSE_ENDPOINT  The same as --endpoint

Example:
  pulumi preview --json -s prod | qloudpulse estimate > comment.md";

#[derive(Subcommand)]
enum Command {
    /// Price an infrastructure change before it is applied, from `pulumi preview --json`.
    ///
    /// Prints a Markdown comment for the pull request. Which resource types and attributes are sent is qloudpulse's
    /// catalogue, read from the endpoint on every run (no token needed), or from --catalogue.
    #[command(after_help = format!("{ESTIMATE_ENVIRONMENT}\n\n{CONNECT}"))]
    Estimate(EstimateArgs),
}

#[derive(clap::Args)]
struct EstimateArgs {
    /// The JSON of `pulumi preview --json`. Without it, or with -, it is read from stdin.
    file: Option<PathBuf>,

    /// Print the request that would be sent, and send nothing. Needs no token.
    #[arg(long)]
    dry_run: bool,

    /// Where to send the request. Its catalogue is read from this address and /catalogue.
    #[arg(long, env = "QLOUDPULSE_ENDPOINT", default_value = send::DEFAULT_ENDPOINT)]
    endpoint: String,

    /// Read the catalogue of priced types from this file instead of the endpoint, such as one saved with curl.
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

fn estimate(args: &EstimateArgs) -> Result<()> {
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
        None => send::fetch_catalogue(&args.endpoint)?,
    };
    // Below the oldest client qloudpulse takes, stop before anything is sent; a newer release is said on stderr and
    // under the comment, where a CI log nobody reads cannot hide it.
    let newer = version::check(version::VERSION, catalogue.client.as_ref())?;
    if let Some(line) = &newer {
        eprintln!("qloudpulse: {line}");
    }
    let estimate = pulumi::estimate(&preview, &catalogue, &options);
    let request = &estimate.request;

    let not_priced: u32 = request.not_priced.iter().map(|n| n.count).sum();
    let same = request
        .resources
        .iter()
        .filter(|r| r.change == ChangeKind::Same)
        .count();
    let referred = if same > 0 {
        format!(" (and {same} unchanged that they refer to)")
    } else {
        String::new()
    };
    eprintln!(
        "qloudpulse: {} resources to price{referred}, {} not priced",
        request.resources.len() - same,
        not_priced
    );

    if args.dry_run {
        println!("{}", serde_json::to_string_pretty(request)?);
        return Ok(());
    }

    let response = if request.resources.len() == same {
        Response {
            format_version: FORMAT_VERSION,
            estimate_id: None,
            currency: "USD".into(),
            resources: vec![],
        }
    } else {
        let token = std::env::var("QLOUDPULSE_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .context(
                "set QLOUDPULSE_TOKEN to a change-event source's token (qpev_...), from Settings, Events in \
                 qloudpulse, or use --dry-run",
            )?;
        send::send(&args.endpoint, token.trim(), request)?
    };

    match args.format {
        Format::Markdown => {
            print!("{}", render::comment(&response, &estimate.locals, &request.not_priced));
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
