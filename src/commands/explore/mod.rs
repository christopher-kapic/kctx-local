//! `kcl explore` — codebase-navigation toolkit.
//!
//! These commands are invoked by the harness that `kcl ask` spawns. The whole
//! surface is hidden from the top-level `kcl --help` (see `cli::Command::Explore`)
//! so the main agent's context isn't polluted by the long list of subcommands;
//! the harness discovers them via `kcl explore --help` and the toolkit block
//! injected into its prompt.

mod circular;
mod deps;
mod hot;
mod impact;
mod outline;
mod read;
mod search;
mod symbol;
mod tree;
pub(crate) mod util;
mod word;

use anyhow::Result;

use crate::cli::ExploreCommand;

/// Dispatch a single `kcl explore` invocation.
pub async fn run(cmd: &ExploreCommand) -> Result<i32> {
    match cmd {
        ExploreCommand::Tree {
            path,
            depth,
            package,
            json,
            max_bytes,
        } => tree::run(
            path.as_deref(),
            *depth,
            package.as_deref(),
            *json,
            *max_bytes,
        ),

        ExploreCommand::Outline {
            file,
            package,
            json,
            max_bytes,
        } => outline::run(file, package.as_deref(), *json, *max_bytes),

        ExploreCommand::Symbol {
            name,
            prefix,
            kind,
            package,
            json,
            max_bytes,
        } => symbol::run(
            name,
            *prefix,
            kind.as_deref(),
            package.as_deref(),
            *json,
            *max_bytes,
        ),

        ExploreCommand::Search {
            pattern,
            type_filter,
            path_glob,
            context,
            case_insensitive,
            package,
            json,
            max_bytes,
        } => {
            search::run(
                pattern,
                type_filter.as_deref(),
                path_glob.as_deref(),
                *context,
                *case_insensitive,
                package.as_deref(),
                *json,
                *max_bytes,
            )
            .await
        }

        ExploreCommand::Word {
            token,
            ignore_case,
            package,
            json,
            max_bytes,
        } => word::run(token, *ignore_case, package.as_deref(), *json, *max_bytes),

        ExploreCommand::Read {
            file,
            start,
            end,
            no_line_numbers,
            package,
            json,
            max_bytes,
        } => read::run(
            file,
            *start,
            *end,
            *no_line_numbers,
            package.as_deref(),
            *json,
            *max_bytes,
        ),

        ExploreCommand::Deps {
            file,
            hops,
            direction,
            package,
            json,
            max_bytes,
        } => deps::run(
            file,
            *hops,
            direction.as_deref(),
            package.as_deref(),
            *json,
            *max_bytes,
        ),

        ExploreCommand::Impact {
            symbol,
            hops,
            file,
            package,
            json,
            max_bytes,
        } => impact::run(
            symbol,
            *hops,
            file.as_deref(),
            package.as_deref(),
            *json,
            *max_bytes,
        ),

        ExploreCommand::Hot {
            limit,
            package,
            json,
            max_bytes,
        } => hot::run(*limit, package.as_deref(), *json, *max_bytes),

        ExploreCommand::Circular {
            package,
            json,
            max_bytes,
        } => circular::run(package.as_deref(), *json, *max_bytes),
    }
}
