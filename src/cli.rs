use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "kcl",
    version,
    about = "Local code knowledge CLI",
    long_about = "kcl gives agents and humans instant Q&A access to any codebase on the local machine.\n\nRegister packages (local paths or git repos), then ask questions. kcl invokes your\npreferred coding harness (Claude Code, opencode, copilot, etc.) in non-interactive\nmode against the codebase and streams the answer back."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Ask a question about a package
    Ask {
        /// Package identifier
        identifier: String,

        /// The question to ask
        question: String,

        /// Override harness for this query
        #[arg(long)]
        harness: Option<String>,

        /// Model to forward to the harness (e.g. claude-sonnet-4.6).
        /// Silently ignored if the selected harness has no model_args
        /// configured.
        #[arg(long)]
        model: Option<String>,

        /// Override timeout in seconds
        #[arg(long)]
        timeout: Option<u64>,

        /// Skip auto-pull even if enabled for this package
        #[arg(long)]
        no_pull: bool,

        /// Check out this branch before answering, then restore the
        /// previously checked-out branch when finished. Pulls the branch
        /// before running the harness unless --no-pull is set.
        #[arg(long)]
        branch: Option<String>,

        /// Include summaries of last N conversations in prompt
        #[arg(long, default_value = "0")]
        context: u32,
    },

    /// Prepare a compact, high-signal orientation map for a package.
    ///
    /// Runs the harness once with a special prompt that asks it to describe
    /// where important things live (directories, entry points, data models,
    /// build commands, etc.) in the fewest tokens possible while remaining
    /// maximally useful for future `kcl ask` sessions.
    ///
    /// The resulting map is stored and (by default) injected into every
    /// subsequent `kcl ask` for this package. When the map is fresh, the ask
    /// prompt instructs the agent to treat it as authoritative and skip a
    /// broad tree scan, which can substantially reduce the initial
    /// exploration phase on large or unfamiliar codebases. The benefit
    /// varies by codebase and harness; small or already well-structured
    /// repos may see little gain.
    ///
    /// Re-run this command after major refactors to refresh the map.
    Prepare {
        /// Package identifier
        identifier: String,

        /// Override harness for this preparation run
        #[arg(long)]
        harness: Option<String>,

        /// Model forwarded to the harness (if the harness supports it)
        #[arg(long)]
        model: Option<String>,

        /// Override timeout in seconds for the preparation run
        #[arg(long)]
        timeout: Option<u64>,

        /// Skip auto-pull even if enabled for this package
        #[arg(long)]
        no_pull: bool,

        /// Check out this branch before preparing the map, then restore
        /// the previously checked-out state. The map records the branch
        /// and commit it was generated against.
        #[arg(long)]
        branch: Option<String>,
    },

    /// List registered packages (alias for packages list)
    List {
        /// Verbose output (identifier, path, source type)
        #[arg(short, long)]
        verbose: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Manage registered packages
    Packages {
        #[command(subcommand)]
        command: PackagesCommand,
    },

    /// View conversation history
    History {
        #[command(subcommand)]
        command: HistoryCommand,
    },

    /// View/edit configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Inspect configured harnesses
    Harnesses {
        #[command(subcommand)]
        command: HarnessesCommand,
    },

    /// Delete on-disk clones of git packages not asked about in N days (re-cloned on demand).
    Prune {
        /// Prune clones with no activity in the last N days
        #[arg(long, default_value = "30")]
        days: u32,

        /// Show what would be pruned without deleting anything
        #[arg(long)]
        dry_run: bool,
    },

    /// Print agent-oriented guidance for using kcl (how to ask, plus per-area
    /// instructions). Run bare for an overview, or pass a topic for detail.
    Agents {
        /// Optional topic: add, remove, prune, config. Omit for the overview.
        topic: Option<AgentsTopic>,
    },

    /// Initialize kcl (creates config + db)
    Init {
        /// Skip prompts, use auto-detected defaults
        #[arg(long)]
        non_interactive: bool,
    },

    /// Codebase-navigation toolkit invoked by the harness that `kcl ask`
    /// spawns. Hidden from the top-level `kcl --help` so it does not pollute
    /// the main agent's context; `kcl explore --help` lists the full toolbox.
    #[command(
        hide = true,
        alias = "x",
        long_about = "Codebase-navigation primitives intended for the harness invoked by `kcl ask`.\n\nEvery command operates on the package in the current working directory unless\n`--package <id>` is given. Add `--json` for structured output and `--max-bytes`\nto cap response size."
    )]
    Explore {
        #[command(subcommand)]
        command: ExploreCommand,
    },
}

/// Topic for `kcl agents <topic>` — selects which area of agent guidance to
/// print. Omitting the topic prints the overview.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum AgentsTopic {
    /// Registering packages (`kcl packages add`).
    Add,
    /// Removing packages (`kcl packages remove` / `rm`).
    Remove,
    /// Reclaiming disk for unused clones (`kcl prune`).
    Prune,
    /// What to do when kcl fails (harness / config issues).
    Config,
}

/// Subcommands of `kcl explore`.
///
/// Common flags accepted by every variant:
/// - `--package <id>` optional override; otherwise the package containing
///   `$PWD` is detected (falling back to `$PWD` itself).
/// - `--json` structured output.
/// - `--max-bytes <n>` truncate output at N bytes (default 16384).
#[derive(Subcommand)]
pub enum ExploreCommand {
    /// Annotated directory tree (no file contents).
    Tree {
        /// Subdirectory to walk (defaults to the package root).
        path: Option<PathBuf>,

        /// Maximum walk depth (uncapped by default).
        #[arg(long)]
        depth: Option<usize>,

        /// Optional package id override.
        #[arg(long)]
        package: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Truncate output at N bytes.
        #[arg(long, default_value = "16384")]
        max_bytes: usize,
    },

    /// Symbol outline (functions, types, imports) for a file.
    Outline {
        /// File to outline.
        file: PathBuf,

        /// Optional package id override.
        #[arg(long)]
        package: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Truncate output at N bytes.
        #[arg(long, default_value = "16384")]
        max_bytes: usize,
    },

    /// Find definition sites of a symbol across the package.
    Symbol {
        /// Symbol name to look up.
        name: String,

        /// Match `name` as a prefix.
        #[arg(long)]
        prefix: bool,

        /// Filter by symbol kind (e.g. `function`, `method`, `struct`, `class`,
        /// `interface`, `type`, `const`, `enum`, `trait`, `module`).
        #[arg(long)]
        kind: Option<String>,

        /// Optional package id override.
        #[arg(long)]
        package: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Truncate output at N bytes.
        #[arg(long, default_value = "16384")]
        max_bytes: usize,
    },

    /// Content search (regex) via ripgrep, budget-capped.
    Search {
        /// Pattern to search for (regex).
        pattern: String,

        /// Restrict by ripgrep file-type alias (passed to `rg --type`).
        #[arg(long = "type", value_name = "TYPE")]
        type_filter: Option<String>,

        /// Restrict by path glob (passed to `rg --glob`).
        #[arg(long = "glob", value_name = "GLOB")]
        path_glob: Option<String>,

        /// Lines of context before and after each match.
        #[arg(long, default_value = "2")]
        context: usize,

        /// Case-insensitive search (passed to `rg --ignore-case`).
        #[arg(long, short = 'i')]
        case_insensitive: bool,

        /// Optional package id override.
        #[arg(long)]
        package: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Truncate output at N bytes.
        #[arg(long, default_value = "16384")]
        max_bytes: usize,
    },

    /// Exact-identifier inverted index lookup.
    Word {
        /// Token to look up.
        token: String,

        /// Case-insensitive match.
        #[arg(long, short = 'i')]
        ignore_case: bool,

        /// Optional package id override.
        #[arg(long)]
        package: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Truncate output at N bytes.
        #[arg(long, default_value = "16384")]
        max_bytes: usize,
    },

    /// Read a line range from a file, with a content hash.
    Read {
        /// File to read.
        file: PathBuf,

        /// First line to include (1-indexed, default 1).
        #[arg(long)]
        start: Option<usize>,

        /// Last line to include (1-indexed, default last).
        #[arg(long)]
        end: Option<usize>,

        /// Omit line-number prefixes.
        #[arg(long)]
        no_line_numbers: bool,

        /// Optional package id override.
        #[arg(long)]
        package: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Truncate output at N bytes.
        #[arg(long, default_value = "16384")]
        max_bytes: usize,
    },

    /// File-level import graph (forward + reverse).
    Deps {
        /// File to look up.
        file: PathBuf,

        /// Number of hops to traverse.
        #[arg(long)]
        hops: Option<usize>,

        /// Direction to walk: `forward`, `reverse`, or `both`.
        #[arg(long)]
        direction: Option<String>,

        /// Optional package id override.
        #[arg(long)]
        package: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Truncate output at N bytes.
        #[arg(long, default_value = "16384")]
        max_bytes: usize,
    },

    /// Symbol-level blast radius.
    ///
    /// Matches are name-based: when several symbols share the same name,
    /// callsite results may include unrelated references. Use `--file` to
    /// scope callers to a specific file or directory when disambiguating.
    Impact {
        /// Symbol name.
        symbol: String,

        /// Number of hops to traverse.
        #[arg(long)]
        hops: Option<usize>,

        /// Restrict callsite matches to a specific file or directory
        /// (path relative to the package root). When set, references whose
        /// `caller_file` is not equal to nor a descendant of this path are
        /// dropped. Useful when a symbol name is ambiguous.
        #[arg(long)]
        file: Option<PathBuf>,

        /// Optional package id override.
        #[arg(long)]
        package: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Truncate output at N bytes.
        #[arg(long, default_value = "16384")]
        max_bytes: usize,
    },

    /// Most-recently-modified files.
    Hot {
        /// Maximum number of entries.
        #[arg(long, default_value = "20")]
        limit: usize,

        /// Optional package id override.
        #[arg(long)]
        package: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Truncate output at N bytes.
        #[arg(long, default_value = "16384")]
        max_bytes: usize,
    },

    /// Circular dependency detection.
    Circular {
        /// Optional package id override.
        #[arg(long)]
        package: Option<String>,

        /// Output as JSON.
        #[arg(long)]
        json: bool,

        /// Truncate output at N bytes.
        #[arg(long, default_value = "16384")]
        max_bytes: usize,
    },
}

#[derive(Subcommand)]
pub enum PackagesCommand {
    /// List registered packages
    List {
        /// Verbose output
        #[arg(short, long)]
        verbose: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Add a package
    Add {
        /// Package identifier (slug)
        identifier: String,

        /// Local path to the codebase
        #[arg(long)]
        path: Option<String>,

        /// Use the current working directory as the package path
        /// (shorthand for `--path "$(pwd)"`).
        #[arg(long, conflicts_with = "path")]
        current_path: bool,

        /// Git URL to clone
        #[arg(long)]
        git: Option<String>,

        /// Git branch to clone (default: the remote's default branch)
        #[arg(long)]
        branch: Option<String>,

        /// Clone with --depth 1 --no-single-branch (saves disk, still allows
        /// checking out other branches later, but truncates history).
        /// See `kcl packages show` for the recorded value and limitations.
        #[arg(long)]
        shallow: bool,
    },

    /// Remove a package
    #[command(visible_alias = "rm")]
    Remove {
        /// Package identifier
        identifier: String,
    },

    /// Show package details
    Show {
        /// Package identifier
        identifier: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Pull latest changes for a package
    Pull {
        /// Package identifier (omit for --all)
        identifier: Option<String>,

        /// Pull all auto-pull packages
        #[arg(long)]
        all: bool,
    },

    /// Set a package property
    Set {
        /// Package identifier
        identifier: String,

        /// Property name (auto-pull, harness, prepare-scope, shallow)
        key: String,

        /// Property value
        value: Option<String>,

        /// Unset the property
        #[arg(long)]
        unset: bool,
    },

    /// Export registered git packages as a JSON manifest (writes to stdout).
    /// Local packages are skipped because their absolute paths are not
    /// reproducible on another machine.
    Export,

    /// Import packages from a JSON manifest produced by `kcl packages export`.
    /// Reads from the given file, or from stdin if no file is given (or `-`).
    /// Existing identifiers are skipped; failures on individual entries do
    /// not abort the batch.
    Import {
        /// Manifest file (omit or pass `-` to read stdin)
        file: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum HistoryCommand {
    /// List recent conversations for a package
    List {
        /// Package identifier
        identifier: String,

        /// Filter by number of days
        #[arg(long)]
        since: Option<u32>,

        /// Limit results
        #[arg(long, default_value = "20")]
        limit: u32,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Show full conversation log
    Show {
        /// Conversation ID
        id: String,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Print current config
    Show {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Open config in $EDITOR
    Edit,

    /// Set a config value (dot-notation key)
    Set {
        /// Config key
        key: String,

        /// Config value
        value: String,
    },

    /// Print config file path
    Path,
}

#[derive(Subcommand)]
pub enum HarnessesCommand {
    /// List configured harnesses
    List {
        /// Verbose output (command, prompt mode, model_args, default_model)
        #[arg(short, long)]
        verbose: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}
