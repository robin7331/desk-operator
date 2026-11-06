//! Command-line interface. Every command returns a [`Report`] so `--json` and human output can
//! never drift apart.

mod device;
mod library;
mod ops;
mod port;
mod power;
mod provider;
mod schema;
mod skill;
mod status;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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
Every device works the same way: `desk get <device>` shows its values and settings, `desk set`
changes settings, `desk do` runs actions, `desk watch` streams; `desk schema --json` lists them.
Rules in short: change things only when the user asked; `desk power` is a device's mains,
`desk set psu output on` a power supply's terminals; never pick values yourself.";

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
    /// Read a device's values and settings: all, or the named ones (`desk get psu
    /// measured_voltage`)
    Get {
        /// Device id, alias or name; `device:channel` for one channel
        device: String,
        /// Values or settings to show [default: all]
        names: Vec<String>,
    },
    /// Change settings, e.g. `desk set psu voltage 5v current 1a output on` or `desk set iron
    /// setpoint 330c`. Units are required; several settings apply in the device's safe order
    Set {
        /// Device id, alias or name; `device:channel` for one channel
        device: String,
        /// Setting and value pairs: `<name> <value> [<name> <value>...]` (a negative value goes
        /// after `--`: `desk set dev -- offset -1v`)
        #[arg(required = true, num_args = 2.., value_names = ["NAME", "VALUE"])]
        pairs: Vec<String>,
        /// Power supplies: switch the output on although a protection (OVP, OCP, ...) tripped,
        /// which clears it
        #[arg(long)]
        clear_protection: bool,
        /// Check ranges and limits and show what would be sent, without sending anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Run an action of a device, e.g. `desk do iron standby` or `desk do iron boost seconds=10s`
    Do {
        /// Device id, alias or name; `device:channel` for one channel
        device: String,
        /// The action
        action: String,
        /// Arguments as name=value (every declared argument is required)
        args: Vec<String>,
        /// Check the arguments and show what would be done, without doing it
        #[arg(long)]
        dry_run: bool,
    },
    /// Stream readings until stopped (Ctrl-C); with --json one JSON line each. Other commands
    /// keep working meanwhile (a background daemon holds the session)
    Watch {
        /// Device id, alias or name; `device:channel` for one channel
        device: String,
        /// Values or settings to stream [default: all]
        names: Vec<String>,
        /// Time between readings ("0": as fast as the device answers)
        #[arg(long, default_value = "100ms", value_name = "DURATION")]
        interval: String,
        /// Stop after this many readings
        #[arg(long, value_name = "N")]
        count: Option<u64>,
        /// Only print a reading when a watched value changed (more than its tolerance)
        #[arg(long)]
        changes: bool,
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
    /// Run the background daemon that keeps device sessions open for `desk watch`.
    /// Started automatically; while it runs, `desk power|get|set|do` go through it
    Serve {
        /// Exit after this long without connections or sessions ("never": run until stopped)
        #[arg(
            long,
            env = "DESK_DAEMON_IDLE",
            default_value = "30s",
            value_name = "DURATION"
        )]
        idle: String,
    },
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
        /// The capability: `voltage_source` (a power supply), `power_switch` (mains), or any
        /// other name for a declared capability (e.g. `heater`, `meter`)
        #[arg(long)]
        capability: String,
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
        Command::Serve { idle } => {
            let idle = match idle.as_str() {
                "never" => None,
                d => Some(parse_duration("--idle", d)?),
            };
            return crate::daemon::serve(cli.config.as_deref(), idle).await;
        }
        Command::Watch {
            ref device,
            interval,
            count,
            ..
        } => {
            let watched = async {
                parse_interval(&interval)?;
                if count == Some(0) {
                    return Err(DeskError::Usage("--count must be at least 1".into()));
                }
                // Units for the human table (the daemon checks everything else).
                let cat = if cli.json {
                    None
                } else {
                    Bench::load(cli.config.as_deref()).ok().and_then(|b| {
                        let d = b
                            .config
                            .resolve_device(crate::ops::split_channel(device).0)
                            .ok()?;
                        crate::ops::Catalog::of(d).ok()
                    })
                };
                let args = crate::daemon::own_args();
                crate::daemon::watch(cli.config.as_deref(), &args, cli.json, count, cat).await
            };
            // A stream's error is one more JSON line, like the ones before it.
            return Ok(watched
                .await
                .unwrap_or_else(|e| Report::streamed(Some((e.to_json(), e.exit_code())))));
        }
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
    // A running daemon may hold the device's session: then it runs the command.
    if routed(&cli.command)
        && let Some(report) =
            crate::daemon::forward(cli.config.as_deref(), &crate::daemon::own_args()).await?
    {
        return Ok(report);
    }
    let bench = Bench::load(cli.config.as_deref())?;
    dispatch(&bench, cli.command).await
}

/// Commands that go through the daemon while it runs: everything touching a device's port or
/// mains (a mains cut ends a session cleanly).
pub fn routed(command: &Command) -> bool {
    matches!(
        command,
        Command::Power { .. } | Command::Get { .. } | Command::Set { .. } | Command::Do { .. }
    )
}

/// `--interval` of `watch`.
pub fn parse_interval(s: &str) -> Result<std::time::Duration> {
    parse_duration("--interval", s)
}

fn parse_duration(what: &str, s: &str) -> Result<std::time::Duration> {
    humantime::parse_duration(s.trim()).map_err(|e| {
        DeskError::Usage(format!(
            "{what}: `{s}` is not a duration like 100ms or 2s: {e}"
        ))
    })
}

/// Run a command that needs the bench (in-process, or inside the daemon).
pub async fn dispatch(bench: &Bench, command: Command) -> Result<Report> {
    match command {
        Command::Schema
        | Command::Skill(_)
        | Command::Port { .. }
        | Command::Library { .. }
        | Command::Serve { .. } => Err(DeskError::Usage(
            "this command doesn't need the bench; run it directly".into(),
        )),
        Command::Power { action } => power::run(bench, action).await,
        Command::Get { device, names } => ops::get(bench, &device, &names).await,
        Command::Set {
            device,
            pairs,
            clear_protection,
            dry_run,
        } => ops::set(bench, &device, &pairs, clear_protection, dry_run).await,
        Command::Do {
            device,
            action,
            args,
            dry_run,
        } => ops::act(bench, &device, &action, &args, dry_run).await,
        Command::Watch { .. } => Err(DeskError::Usage(
            "`desk watch` streams through the daemon; it can't run here".into(),
        )),
        Command::Device { action } => device::run(bench, action),
        Command::Provider { action } => provider::run(bench, action).await,
        Command::Status => status::run(bench).await,
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
