mod cli;
mod commands;
mod config;
mod db;
mod explore_index;
mod git;
mod harness;
mod models;
mod paths;

use clap::Parser;

use cli::{Cli, Command};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match &cli.command {
        Command::Ask {
            identifier,
            question,
            harness,
            model,
            timeout,
            no_pull,
            branch,
            context,
        } => {
            commands::ask::run(commands::ask::AskArgs {
                identifier,
                question,
                harness_override: harness.as_deref(),
                model: model.as_deref(),
                timeout_override: *timeout,
                no_pull: *no_pull,
                branch_override: branch.as_deref(),
                context: *context,
            })
            .await
        }

        Command::List { verbose, json } => {
            let cmd = cli::PackagesCommand::List {
                verbose: *verbose,
                json: *json,
            };
            commands::packages::run(&cmd).await.map(|()| 0)
        }

        Command::Packages { command } => commands::packages::run(command).await.map(|()| 0),

        Command::History { command } => commands::history::run(command).map(|()| 0),

        Command::Config { command } => commands::config_cmd::run(command).map(|()| 0),

        Command::Harnesses { command } => commands::harnesses::run(command).map(|()| 0),

        Command::Prune { days, dry_run } => commands::prune::run(*days, *dry_run).map(|()| 0),

        Command::Agents { topic } => commands::agents::run(*topic).map(|()| 0),

        Command::Init { non_interactive } => commands::init::run(*non_interactive).map(|()| 0),

        c @ Command::Prepare { .. } => commands::prepare::run(c).await,

        Command::Explore { command } => commands::explore::run(command).await,
    };

    match result {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {:#}", e);
            std::process::exit(1);
        }
    }
}
