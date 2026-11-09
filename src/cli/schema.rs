//! `desk schema`: everything an agent needs to operate this bench, in one document.
//!
//! The command list is generated from the clap definitions, so it can't drift from the real CLI.
//! It works without a valid config, and then reports the config problem inline.

use std::path::Path;

use clap::{ArgAction, CommandFactory};
use serde::Serialize;
use serde_json::{Value, json};

use super::Cli;
use super::device::DeviceView;
use crate::bench::Bench;
use crate::error::{ERROR_CODES, Result};
use crate::output::Report;

#[derive(Serialize)]
struct CommandInfo {
    command: String,
    about: String,
    args: Vec<ArgInfo>,
}

#[derive(Serialize)]
struct ArgInfo {
    name: String,
    /// `positional`, `flag` (no value) or `option` (takes a value)
    kind: &'static str,
    required: bool,
    multiple: bool,
    help: String,
}

/// `config_path`: the `--config` given, if any (a missing file is normal on a first run, so the
/// path is reported even then).
pub fn run(bench: Result<Bench>, config_path: Option<&Path>) -> Report {
    let (config, devices, providers) = match &bench {
        Ok(b) => (
            json!({ "ok": true, "path": b.config.path }),
            b.config
                .devices
                .iter()
                .map(DeviceView::new)
                .collect::<Vec<_>>(),
            b.providers
                .ids()
                .map(|(id, cfg)| json!({ "id": id, "kind": cfg.kind }))
                .collect::<Vec<_>>(),
        ),
        Err(e) => (
            json!({
                "ok": false,
                "path": config_path.map(Path::to_path_buf).or_else(crate::config::default_path),
                "error": e.to_json(),
            }),
            Vec::new(),
            Vec::new(),
        ),
    };

    let root = Cli::command();
    let mut commands = Vec::new();
    walk(&root, "desk", &mut commands);

    let data = json!({
        "desk": { "version": env!("CARGO_PKG_VERSION") },
        "config": config,
        "devices": devices,
        "providers": providers,
        "capabilities": {
            "power_switch": {
                "description": "Mains power of a device (not a PSU's output terminals).",
                "commands": ["desk power on|off|toggle <device>... [--dry-run]", "desk power status [<device>...]"],
                "states": ["on", "off", "unknown"],
                "confidence": {
                    "real": "read back from the device or provider",
                    "assumed": "inferred from the command just sent; the path cannot read its state",
                    "none": "state is unknown"
                }
            },
            "voltage_source": {
                "description": "A bench power supply (capability key `source`). Its names are fixed: settings voltage, current, output; values measured_voltage, measured_current, measured_power, and if reported mode (cv|cc), protection (none|ovp|ocp|opp|otp|uvp|other), temperature, input_voltage, charge, energy.",
                "examples": [
                    "desk set psu voltage 5v current 1a output on",
                    "desk set psu output off",
                    "desk get psu",
                    "desk watch psu measured_current --interval 1s"
                ],
                "rules": "setpoints are applied before the output, whatever order they're typed in; output on checks the setpoints already on the device against the limits; output on is refused while a protection is tripped (protection_tripped) unless --clear-protection",
                "requires": "the device's mains must be on (device_off otherwise)"
            },
            "declared": {
                "description": "Any other capability: the model declares its settings, values and actions (see devices[].settings/values/actions, with types, units, ranges, limits and `switches`).",
                "commands": [
                    "desk get <device>[:<channel>] [<name>...]",
                    "desk set <device>[:<channel>] <setting> <value> [<setting> <value>...] [--dry-run]",
                    "desk do <device>[:<channel>] <action> [<arg>=<value>...] [--dry-run]",
                    "desk watch <device>[:<channel>] [<name>...] [--interval 100ms] [--count N] [--changes]"
                ],
                "rules": "numbers need their unit (330c, 250ma, 1.5kohm; prefixes are case-sensitive: m milli, M mega); the device range (min/max) and the bench limit are checked before anything is sent (limit_exceeded); several settings apply in declaration order; settings are read back (verified: true, absent = the device can't report it); `switches: true` means a physical effect: ask the user first",
                "values": "numbers come in the declared unit without prefixes; a value declared with `units` comes as { value, unit }; absent = not reported",
                "left_out": "`desk set <device>` and `desk do <device>` list what's possible; `desk set <device> <setting>` and `desk do <device> <action>` (if it needs arguments) describe that one, with the setting's current value if it could be read (value, or value_error). All of them change nothing and end as a usage error, with the listing in data"
            }
        },
        "commands": commands,
        "global_args": args_of(&root),
        "output": {
            "json": "with --json, stdout is one document: { ok, data, error? }; error = { code, message, hint? }",
            "partial_failure": "multi-device commands report every device in data.results; ok=false and error/exit code come from the first failure",
            "logs": "script bindings may return data.results[].log; they also print to stderr",
            "streams": "`desk watch --json` prints one { ok, data } line per reading (data: device, time, elapsed, values, channels?, dropped, failed) and a final { ok: false, error } line if it ends with an error; a background daemon holds the session, other commands keep working"
        },
        "errors": ERROR_CODES
            .iter()
            .map(|(code, exit, meaning)| json!({ "code": code, "exit": exit, "meaning": meaning }))
            .collect::<Vec<Value>>(),
    });

    let human = serde_json::to_string_pretty(&data).unwrap_or_default() + "\n";
    Report::new(&data, human)
}

fn walk(cmd: &clap::Command, path: &str, out: &mut Vec<CommandInfo>) {
    for sub in cmd.get_subcommands() {
        if sub.get_name() == "help" || sub.is_hide_set() {
            continue;
        }
        let path = format!("{path} {}", sub.get_name());
        if sub.has_subcommands() {
            walk(sub, &path, out);
        }
        // A group that also takes arguments of its own (`desk skill [name]`) is a command too.
        if !sub.has_subcommands() || sub.get_positionals().next().is_some() {
            out.push(CommandInfo {
                command: path,
                about: sub.get_about().map(|a| a.to_string()).unwrap_or_default(),
                args: args_of(sub),
            });
        }
    }
}

fn args_of(cmd: &clap::Command) -> Vec<ArgInfo> {
    cmd.get_arguments()
        .filter(|a| !a.is_hide_set() && !matches!(a.get_id().as_str(), "help" | "version"))
        .map(|a| {
            let kind = if a.is_positional() {
                "positional"
            } else if a.get_action().takes_values() {
                "option"
            } else {
                "flag"
            };
            let name = match a.get_long() {
                Some(long) => format!("--{long}"),
                None => a.get_id().to_string(),
            };
            ArgInfo {
                name,
                kind,
                required: a.is_required_set(),
                multiple: matches!(a.get_action(), ArgAction::Append)
                    || a.get_num_args().is_some_and(|n| n.max_values() > 1),
                help: a.get_help().map(|h| h.to_string()).unwrap_or_default(),
            }
        })
        .collect()
}
