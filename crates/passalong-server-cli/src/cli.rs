//! The command line, as `clap` sees it. `docs/usage.md` is its manual.

use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand};

/// Where the source is. A modified server that others use over a network
/// must offer them its own source (AGPL, section 13): whoever distributes or
/// runs a changed version changes this to where theirs is.
pub const SOURCE_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// Under every help screen, at every level.
fn footer() -> String {
    format!(
        "Source:  {SOURCE_URL}\nLicence: {}, free software with NO WARRANTY; see LICENSE.",
        env!("CARGO_PKG_LICENSE")
    )
}

fn with_footer(command: clap::Command) -> clap::Command {
    command.after_help(footer()).mut_subcommands(with_footer)
}

/// The command line, with the footer under every command's help.
pub fn command() -> clap::Command {
    with_footer(Cli::command())
}

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
    Check {
        /// Instead, ask the running server whether it is ready: exit 0 if
        /// so, 1 if not. For a container's health check.
        #[arg(long)]
        health: bool,
    },
    /// Run the server, until SIGTERM or Ctrl-C.
    Serve,
    /// Make a self-signed certificate, or print a certificate's pin.
    #[command(subcommand)]
    Tls(TlsCommand),
    /// Install the server as a systemd system service, or remove it.
    #[command(subcommand)]
    Service(ServiceCommand),
}

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// As root: install this binary, create the service's user and its
    /// directories, write a configuration if there is none, and write,
    /// enable, and start the unit. Overwrites nothing that is yours.
    Install {
        /// Make a self-signed certificate for this name if there is no
        /// certificate yet. May be given more than once.
        #[arg(long, value_name = "NAME")]
        host: Vec<String>,
        /// As --host, for an address.
        #[arg(long, value_name = "ADDR")]
        ip: Vec<std::net::IpAddr>,
    },
    /// As root: stop, disable, and remove the unit. Data, configuration,
    /// certificate, user, and binary stay.
    Remove,
}

#[derive(Debug, Subcommand)]
pub enum TlsCommand {
    /// Write a self-signed certificate and its key to tls.cert_file and
    /// tls.key_file, and print the pin clients connect by.
    SelfSigned {
        /// A name clients will connect to. May be given more than once.
        #[arg(long, value_name = "NAME", required = true)]
        host: Vec<String>,
        /// An address clients will connect to. May be given more than once.
        #[arg(long, value_name = "ADDR")]
        ip: Vec<std::net::IpAddr>,
    },
    /// Print the pin of the certificate in tls.cert_file.
    Fingerprint,
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
