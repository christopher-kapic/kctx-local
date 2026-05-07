use clap::{Parser, Subcommand};

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

    /// Initialize kcl (creates config + db)
    Init {
        /// Skip prompts, use auto-detected defaults
        #[arg(long)]
        non_interactive: bool,
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

        /// Git URL to clone
        #[arg(long)]
        git: Option<String>,

        /// Git branch to clone (default: the remote's default branch)
        #[arg(long)]
        branch: Option<String>,
    },

    /// Remove a package
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

        /// Property name (auto-pull, harness)
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
