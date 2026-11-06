mod bench;
mod cli;
mod config;
mod daemon;
mod declared;
mod error;
mod model;
mod models;
mod ops;
mod output;
mod power;
mod providers;
mod script;
mod serial;
mod source;
mod units;

use clap::error::ErrorKind;
use clap::{CommandFactory, Parser};
use clap_complete::CompleteEnv;

use crate::cli::Cli;
use crate::error::DeskError;

fn main() {
    // A shell asking for completions (`desk completions <shell>` set it up): answers and exits.
    CompleteEnv::with_factory(Cli::command)
        .var(cli::complete::VAR)
        .complete();

    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let informational = matches!(
                e.kind(),
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayVersion
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
            );
            // Agents asked for JSON: give them a parseable usage error too.
            if !informational && std::env::args().any(|a| a == "--json") {
                let msg = e.render().to_string();
                let msg = msg.trim().trim_start_matches("error: ");
                output::emit_error(true, &DeskError::Usage(msg.to_string()));
                std::process::exit(2);
            }
            e.exit();
        }
    };

    let json = cli.json;
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            output::emit_error(
                json,
                &DeskError::Internal(format!("cannot start runtime: {e}")),
            );
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(cli::run(cli));
    std::process::exit(output::emit(json, result));
}
