//! The command line, as `clap` sees it. `docs/usage.md` is its manual.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Self-hosted server for passalong: the operator's commands.
#[derive(Debug, Parser)]
#[command(name = "passalong-server", version, propagate_version = true)]
pub struct Cli {
    /// The configuration file, instead of looking in the usual places.
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Print listings as JSON.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Write a configuration file and create the data directory.
    Init(InitArgs),
    /// Create, list, show, and delete workspaces.
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    /// Create, list, extend, revoke, delete, and prune API keys.
    #[command(subcommand)]
    Key(KeyCommand),
    /// Show or abort a rewrite session a device left open.
    #[command(subcommand)]
    Rewrite(RewriteCommand),
    /// Check the configuration, the data directory, and every workspace.
    Check,
    /// Run the server. Not in this build yet.
    Serve,
    /// TLS certificates. Not in this build yet.
    Tls {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
    /// The systemd service. Not in this build yet.
    Service {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
        rest: Vec<String>,
    },
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Where workspaces and the control database go.
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// Where to write the configuration file.
    #[arg(long, value_name = "FILE")]
    pub config_file: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// Create a workspace.
    Create {
        /// Lower-case letters, digits, and dashes; at most 32; a letter first.
        name: String,
        /// Its quota, such as 20GiB. Default: limits.workspace_quota_bytes.
        #[arg(long, value_name = "SIZE")]
        quota: Option<String>,
    },
    /// List workspaces.
    List,
    /// Show one workspace: items, bytes, encryption, keys.
    Show { name: String },
    /// Delete a workspace with its items and its keys.
    Delete {
        name: String,
        /// Do not ask for the name to be typed again.
        #[arg(long)]
        yes: bool,
        /// Also while a rewrite session is open.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Create a key. It is printed once and cannot be shown again.
    Create {
        #[arg(long, value_name = "NAME")]
        workspace: String,
        /// What the key is for: a device, usually.
        #[arg(long)]
        label: String,
        /// How long it lasts, such as 90d. Default: 90d.
        #[arg(long, value_name = "DURATION", conflicts_with = "never")]
        expires: Option<String>,
        /// It never expires.
        #[arg(long)]
        never: bool,
        /// It may read and never write.
        #[arg(long)]
        read_only: bool,
    },
    /// List keys: never a secret.
    List {
        #[arg(long, value_name = "NAME")]
        workspace: Option<String>,
    },
    /// Move a key's expiry, counted from now; the secret stays.
    Extend {
        id: String,
        #[arg(
            long,
            value_name = "DURATION",
            conflicts_with = "never",
            required_unless_present = "never"
        )]
        expires: Option<String>,
        #[arg(long)]
        never: bool,
    },
    /// Refuse a key from the next request on.
    Revoke { id: String },
    /// Remove a key's record.
    Delete { id: String },
    /// Delete keys that expired or were revoked long ago.
    Prune {
        #[arg(long, value_name = "DURATION")]
        older_than: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum RewriteCommand {
    /// Show the open rewrite session of a workspace.
    Show { workspace: String },
    /// Abort it: the workspace is again what it was before.
    Abort {
        workspace: String,
        /// Also while its holder's lease still runs.
        #[arg(long)]
        force: bool,
    },
}
