//! `desk schema`: everything an agent needs to operate this bench, in one document.
//!
//! The command list is generated from the clap definitions, so it can't drift from the real CLI.
//! It works without a valid config, and then reports the config problem inline.

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

pub fn run(bench: Result<Bench>) -> Report {
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
            json!({ "ok": false, "error": e.to_json() }),
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
                "description": "A bench power supply: setpoints, output terminals, measurements.",
                "commands": [
                    "desk source set <device> <5v> <1a> [--on] [--dry-run]",
                    "desk source output on|off <device> [--dry-run]",
                    "desk source read <device>"
                ],
                "units": "values need units: v, mv, a, ma",
                "safety": "the core refuses values above the device maximum or the device's `limits` (limit_exceeded, nothing sent); setpoints are applied before the output is switched on",
                "verified": "per requested value: true = read back and confirmed; absent = the device can't report it (unconfirmed)",
                "requires": "the device's mains must be on (device_off otherwise)"
            }
        },
        "commands": commands,
        "global_args": args_of(&root),
        "output": {
            "json": "with --json, stdout is one document: { ok, data, error? }; error = { code, message, hint? }",
            "partial_failure": "multi-device commands report every device in data.results; ok=false and error/exit code come from the first failure",
            "logs": "script bindings may return data.results[].log; they also print to stderr"
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
        } else {
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
