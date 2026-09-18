//! `passalong-server`: the daemon and its operations CLI in one binary.
//!
//! Planned commands (see `docs/usage.md`):
//!
//! - `serve`: run the server in the foreground; systemd and Docker run this.
//! - `init`: write a config file and create the data directory.
//! - `workspace create | list | show | delete`
//! - `key create | list | revoke | delete | prune`
//! - `tls self-signed | fingerprint`
//! - `service install | remove`: the hardened systemd system unit.
//! - `check`: validate the config, the data directory, and the TLS files;
//!   `check --health` asks a running server, for Docker's `HEALTHCHECK`.

use std::process::ExitCode;

fn main() -> ExitCode {
    // User-facing CLI output, not an application log.
    eprintln!(
        "passalong-server {}: not implemented yet; see docs/ideas/ and docs/backlog.md",
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::from(2)
}
