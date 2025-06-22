use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "git-train", version, about = "Simple stack diff CLI tool")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a new stack from current branch
    Create {
        /// Stack name
        name: String,
    },

    /// Add current changes to the stack
    Commit {
        /// Commit message
        #[arg(short, long)]
        message: String,
    },

    /// Amend the current commit and resync downstream branches
    Amend {
        /// Updated commit message (optional)
        #[arg(short, long)]
        message: Option<String>,
    },

    /// Add current branch to the stack
    Add {
        /// Parent branch (defaults to current stack's base branch)
        #[arg(short, long)]
        parent: Option<String>,
    },

    /// Show stack status
    Status,

    /// List all stacks
    List,

    /// Switch to a different stack
    Switch {
        /// Stack name or ID
        stack: String,
    },

    /// Interactive navigation through the stack
    Navigate,

    /// Delete a stack
    Delete {
        /// Stack name or ID
        stack: String,
        /// Skip confirmation prompt
        #[arg(short, long)]
        force: bool,
    },

    /// Push stack to remote
    Push,

    /// Sync with remote (pull latest and rebase)
    Sync,

    /// Configuration management
    #[command(subcommand)]
    Config(ConfigCommands),

    /// Check repository and stack health
    Health,
}

#[derive(Subcommand)]
pub enum ConfigCommands {
    /// Show current configuration
    Show,

    /// Configure git-train interactively
    Setup,

    /// Set default editor
    SetEditor {
        /// Editor command (e.g., 'cursor', 'code', 'vim')
        editor: String,
    },

    /// Set conflict resolution strategy
    SetStrategy {
        /// Strategy: 'never', 'simple', or 'smart'
        strategy: String,
    },

    /// Set force-push behavior
    SetForcePush {
        /// Mode: 'auto', 'prompt', or 'never'
        mode: String,
    },
}
