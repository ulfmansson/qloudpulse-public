use std::io::Read as _;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};

use qloudpulse_cli::pulumi::{self, Options};
use qloudpulse_cli::request::{FORMAT_VERSION, Response};
use qloudpulse_cli::{render, send};

#[derive(Parser)]
#[command(name = "qloudpulse", version, about = "qloudpulse from the command line")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Price an infrastructure change before it is applied, from `pulumi preview --json`.
    ///
    /// Prints a Markdown comment for the pull request. The token is read from QLOUDPULSE_TOKEN: a change-event
    /// source's token from Settings, Events.
    Estimate(EstimateArgs),
}

#[derive(clap::Args)]
struct EstimateArgs {
    /// The JSON of `pulumi preview --json`. Without it, or with -, it is read from stdin.
    file: Option<PathBuf>,

    /// Print the request that would be sent, and send nothing. Needs no token.
    #[arg(long)]
    dry_run: bool,

    /// Where to send the request.
    #[arg(long, env = "QLOUDPULSE_ENDPOINT", default_value = send::DEFAULT_ENDPOINT)]
    endpoint: String,

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
    let estimate = pulumi::estimate(&preview, &options);
    let request = &estimate.request;

    let not_priced: u32 = request.not_priced.iter().map(|n| n.count).sum();
    eprintln!(
        "qloudpulse: {} resources to price, {} not priced",
        request.resources.len(),
        not_priced
    );

    if args.dry_run {
        println!("{}", serde_json::to_string_pretty(request)?);
        return Ok(());
    }

    let response = if request.resources.is_empty() {
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
            .context("set QLOUDPULSE_TOKEN to a change-event source's token, or use --dry-run")?;
        send::send(&args.endpoint, token.trim(), request)?
    };

    match args.format {
        Format::Markdown => print!("{}", render::comment(&response, &estimate.locals, &request.not_priced)),
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
