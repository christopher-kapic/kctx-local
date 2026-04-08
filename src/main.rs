mod cli;
mod commands;
mod config;
mod db;
mod dirs;
mod git;
mod harness;
mod models;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Command::Ask {
            identifier,
            question,
            harness,
            model,
            timeout,
            no_pull,
            branch,
            context,
        } => commands::ask::run(commands::ask::AskArgs {
            identifier,
            question,
            harness_override: harness.as_deref(),
            model: model.as_deref(),
            timeout_override: *timeout,
            no_pull: *no_pull,
            branch_override: branch.as_deref(),
            context: *context,
        }),

        Command::List { verbose, json } => {
            // Alias for packages list
            let cmd = cli::PackagesCommand::List {
                verbose: *verbose,
                json: *json,
            };
            commands::packages::run(&cmd)
        }

        Command::Packages { command } => commands::packages::run(command),

        Command::History { command } => commands::history::run(command),

        Command::Config { command } => commands::config_cmd::run(command),

        Command::Harnesses { command } => commands::harnesses::run(command),

        Command::Init { non_interactive } => commands::init::run(*non_interactive),
    }
}
