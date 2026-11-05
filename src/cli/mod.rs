//! Command-line interface. Every command returns a [`Report`] so `--json` and human output can
//! never drift apart.

mod device;
mod library;
mod port;
mod power;
mod provider;
mod schema;
mod skill;
mod source;
mod status;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::bench::Bench;
use crate::error::{DeskError, ErrorJson, Result};
use crate::output::Report;

/// What an agent without a skill needs to find its way: where to start, and the rules.
const AFTER_HELP: &str = "\
Every command accepts --json: stdout is then one document { ok, data, error? }.
`desk help <command>` (or `desk <command> --help`) shows each command's arguments.

For agents:
  desk schema --json        the bench (devices, providers), every command, every error code
  desk skill desk-operator  how to operate the bench safely: read it before switching anything
  desk skill list           all skills (e.g. adding a device desk doesn't know yet)
Rules in short: change things only when the user asked; `desk power` is a device's mains,
`desk source output` a power supply's terminals; never pick voltages or currents yourself.";

#[derive(Debug, Parser)]
#[command(
    name = "desk",
    version,
    about = "Control the equipment on your electronics bench",
    after_help = AFTER_HELP
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
    /// Control bench power supplies: setpoints, output, measurements (voltage_source)
    Source {
        #[command(subcommand)]
        action: SourceCmd,
    },
    /// Inspect configured devices
    Device {
        #[command(subcommand)]
        action: DeviceCmd,
    },
    /// Device definitions: models desk knows (bundled, local, or from $DESK_DEVICES)
    Library {
        #[command(subcommand)]
        action: LibraryCmd,
    },
    /// Serial ports and their USB ids (to fill in a binding's `port = { ... }`)
    Port {
        #[command(subcommand)]
        action: PortCmd,
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
    /// Agent skills (SKILL.md) matching this version of desk: print, list or install them
    Skill(SkillArgs),
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct SkillArgs {
    /// Skill to print [default: desk-operator]
    pub name: Option<String>,

    #[command(subcommand)]
    pub action: Option<SkillCmd>,
}

#[derive(Debug, Subcommand)]
pub enum SkillCmd {
    /// List the skills shipped with desk
    List,
    /// Install every skill for your agent (asks where on a terminal). Replaces only older
    /// versions of desk's skills
    Install(InstallArgs),
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// For every Claude Code session: ~/.claude/skills
    #[arg(long, conflicts_with_all = ["project", "dir"])]
    pub global: bool,
    /// For the current project only: ./.claude/skills
    #[arg(long, conflicts_with = "dir")]
    pub project: bool,
    /// Another skills folder (e.g. another agent's); skills go to <dir>/<name>/SKILL.md
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Also overwrite files that aren't desk's skill of that name
    #[arg(long)]
    pub force: bool,
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
pub enum SourceCmd {
    /// Set voltage and/or current, e.g. `desk source set psu 5v 1a --on`
    Set {
        /// Device id, alias or name
        device: String,
        /// Values with units: 5v, 500mv, 1a, 250ma
        #[arg(required = true)]
        values: Vec<String>,
        /// Then switch the output on (after the setpoints)
        #[arg(long)]
        on: bool,
        /// With --on: switch on although a protection (OVP, OCP, ...) tripped, which clears it
        #[arg(long, requires = "on")]
        clear_protection: bool,
        /// Check limits and show what would be sent, without sending anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Switch the output terminals on or off (not mains power: see `desk power`)
    Output {
        /// on or off
        state: OnOff,
        /// Device id, alias or name
        device: String,
        /// With `on`: switch on although a protection (OVP, OCP, ...) tripped, which clears it
        #[arg(long)]
        clear_protection: bool,
        /// Check limits and show what would be sent, without sending anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Read output state, setpoints, measurements, and whatever else the device reports (mode,
    /// protection, temperature, ...)
    Read {
        /// Device id, alias or name
        device: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum LibraryCmd {
    /// List available device definitions and where they come from
    List,
    /// Show one definition: bindings, status, and how to use it
    Show {
        /// Definition id, e.g. vendor/model
        id: String,
    },
    /// Validate definitions and load their drivers (no hardware is touched)
    Check {
        /// Definition ids or paths to device.toml files [default: all]
        targets: Vec<String>,
    },
    /// Create a new definition: device.toml, a driver skeleton and a vendor README
    New {
        /// Definition id: vendor/model (lowercase, digits, `-`, `_`)
        id: String,
        /// The capability the definition implements
        #[arg(long, value_enum)]
        capability: CapabilityArg,
        /// Human-readable model name, e.g. "ACME PSU 9000"
        #[arg(long)]
        name: Option<String>,
        /// Use an existing driver (path relative to the new device.toml, e.g. ../shared/x.lua)
        #[arg(long)]
        driver: Option<String>,
        /// The device's maximum voltage (voltage_source)
        #[arg(long)]
        max_voltage: Option<f64>,
        /// The device's maximum current (voltage_source)
        #[arg(long)]
        max_current: Option<f64>,
        /// Library folder to create it in [default: devices/ next to the config]
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum CapabilityArg {
    VoltageSource,
    PowerSwitch,
}

#[derive(Debug, Subcommand)]
pub enum PortCmd {
    /// List USB serial ports with vendor/product ids and serial numbers
    List {
        /// Also list non-USB ports (e.g. legacy /dev/ttyS*)
        #[arg(long)]
        all: bool,
    },
    /// Send raw bytes to a serial port and dump the replies as hex (for adding devices).
    /// No limits are checked: send only queries you know are read-only
    Probe(ProbeArgs),
}

#[derive(Debug, Args)]
pub struct ProbeArgs {
    /// Port path, e.g. /dev/ttyACM0 (or select it with --usb / --serial-number)
    pub path: Option<String>,
    /// USB ids as `desk port list` shows them, e.g. 2e3c:5740
    #[arg(long, value_name = "VID:PID")]
    pub usb: Option<String>,
    /// USB serial number of the port
    #[arg(long)]
    pub serial_number: Option<String>,
    /// Baud rate
    #[arg(long, default_value_t = 9600)]
    pub baud: u32,
    /// Data bits, parity (N/E/O), stop bits
    #[arg(long, default_value = "8N1")]
    pub line: String,
    /// Bytes to send, as hex ("F1 A1 DE 01 00 DF"); repeat for several, each followed by a listen
    #[arg(long, value_name = "BYTES")]
    pub send: Vec<String>,
    /// Treat --send values as text with escapes (\n, \r, \t, \\, \xNN), e.g. "*IDN?\n"
    #[arg(long)]
    pub text: bool,
    /// How long to collect replies after each send (or in total without --send), at most 60s
    #[arg(long, default_value = "1s", value_name = "DURATION")]
    pub listen: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OnOff {
    On,
    Off,
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

pub async fn run(cli: Cli) -> Result<Report> {
    // These work without a (valid) config.
    match cli.command {
        Command::Schema => return Ok(schema::run(Bench::load(cli.config.as_deref()))),
        Command::Port { action } => {
            return match action {
                PortCmd::List { all } => port::list(all),
                PortCmd::Probe(args) => port::probe(args).await,
            };
        }
        Command::Library { action } => return library::run(cli.config.as_deref(), action),
        Command::Skill(SkillArgs { name, action }) => return skill::run(name, action, cli.json),
        _ => {}
    }
    let bench = Bench::load(cli.config.as_deref())?;
    match cli.command {
        Command::Schema | Command::Skill(_) | Command::Port { .. } | Command::Library { .. } => {
            unreachable!("handled above")
        }
        Command::Power { action } => power::run(&bench, action).await,
        Command::Source { action } => source::run(&bench, action).await,
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

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    /// Agents without a skill navigate by `desk help`: every command and argument explains itself.
    #[test]
    fn every_command_and_argument_has_help() {
        fn check(cmd: &clap::Command, path: &str, missing: &mut Vec<String>) {
            for sub in cmd.get_subcommands().filter(|s| s.get_name() != "help") {
                let path = format!("{path} {}", sub.get_name());
                if sub.get_about().is_none() {
                    missing.push(path.clone());
                }
                for arg in sub.get_arguments() {
                    if arg.get_help().is_none()
                        && !matches!(arg.get_id().as_str(), "help" | "version")
                    {
                        missing.push(format!("{path} <{}>", arg.get_id()));
                    }
                }
                check(sub, &path, missing);
            }
        }
        Cli::command().debug_assert();
        let mut missing = Vec::new();
        check(&Cli::command(), "desk", &mut missing);
        assert!(missing.is_empty(), "no help text: {missing:?}");
    }
}
