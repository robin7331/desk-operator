//! Command-line interface. Every command returns a [`Report`] so `--json` and human output can
//! never drift apart.

mod device;
mod power;
mod provider;
mod schema;
mod status;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::bench::Bench;
use crate::error::{DeskError, ErrorJson, Result};
use crate::output::Report;

#[derive(Debug, Parser)]
#[command(
    name = "desk",
    version,
    about = "Control the equipment on your electronics bench",
    after_help = "Every command accepts --json for machine-readable output (see `desk help <command>`)."
)]
pub struct Cli {
    /// Print a JSON envelope `{ ok, data, error }` on stdout instead of human output
    #[arg(long, global = true)]
    pub json: bool,

    /// Path to desk.toml [default: platform config dir, e.g. ~/.config/desk-operator/desk.toml]
    #[arg(long, global = true, env = "DESK_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Switch or query mains power of devices
    Power {
        #[command(subcommand)]
        action: PowerCmd,
    },
    /// Inspect configured devices
    Device {
        #[command(subcommand)]
        action: DeviceCmd,
    },
    /// Inspect providers (Home Assistant, ...) and what they can control
    Provider {
        #[command(subcommand)]
        action: ProviderCmd,
    },
    /// Overview of all devices and providers
    Status,
    /// Machine-readable description of the bench, all commands and error codes (for agents)
    Schema,
    /// Print the agent skill (SKILL.md) matching this version of desk
    Skill,
}

#[derive(Debug, Subcommand)]
pub enum PowerCmd {
    /// Switch mains power on
    On(Targets),
    /// Switch mains power off. Without devices, or with `all`: every device
    Off(Targets),
    /// Toggle mains power (needs a path that can read back its state)
    Toggle(Targets),
    /// Show mains power state [default: all devices with a power path]
    Status {
        /// Device ids, aliases or names
        devices: Vec<String>,
    },
}

#[derive(Debug, Args)]
pub struct Targets {
    /// Device ids, aliases or names
    #[arg(conflicts_with = "all")]
    pub devices: Vec<String>,

    /// Every device that has a power path
    #[arg(long)]
    pub all: bool,

    /// Show what would be sent, without switching anything (current state is still read)
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum ProviderCmd {
    /// List configured providers and the devices using them
    List,
    /// Check that a provider is reachable and authenticated
    Test {
        /// Provider id from [providers.<id>]
        provider: String,
    },
    /// List what a provider can switch, e.g. Home Assistant entities, to use in a power binding
    Endpoints {
        /// Provider id from [providers.<id>]
        provider: String,
        /// Only show endpoints whose id or name contains this text (case-insensitive)
        #[arg(long, short)]
        search: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum DeviceCmd {
    /// List configured devices
    List,
    /// Show one device in detail
    Show {
        /// Device id, alias or name
        device: String,
    },
}

/// The agent skill, shipped inside the binary so it always matches this version.
pub const SKILL: &str = include_str!("../../skills/desk-operator/SKILL.md");

pub async fn run(cli: Cli) -> Result<Report> {
    // These work without a (valid) config.
    match cli.command {
        Command::Schema => return Ok(schema::run(Bench::load(cli.config.as_deref()))),
        Command::Skill => {
            return Ok(Report::new(
                &serde_json::json!({ "skill": SKILL }),
                SKILL.to_string(),
            ));
        }
        _ => {}
    }
    let bench = Bench::load(cli.config.as_deref())?;
    match cli.command {
        Command::Schema | Command::Skill => unreachable!("handled above"),
        Command::Power { action } => power::run(&bench, action).await,
        Command::Device { action } => device::run(&bench, action),
        Command::Provider { action } => provider::run(&bench, action).await,
        Command::Status => status::run(&bench).await,
    }
}

/// Summarise per-item failures into the envelope's top-level error (first failure wins the code).
fn summarize_failures(total: usize, failures: &[&DeskError]) -> Option<(ErrorJson, i32)> {
    let first = failures.first()?;
    let mut err = first.to_json();
    if total > 1 {
        err.message = format!(
            "{} of {total} failed; first: {}",
            failures.len(),
            err.message
        );
    }
    Some((err, first.exit_code()))
}
