//! `passalong-server`: the operator's commands and the server, in one
//! binary. `docs/usage.md` is the manual.
//!
//! What a command prints for the operator goes to standard output and is not
//! a log; logs go to standard error (`AGENTS.md`, "Observability").

mod cli;
mod commands;
mod output;
mod owner;
mod serve;

use std::process::ExitCode;

use clap::Parser;
use passalong_server_core::config;

use cli::{Cli, Commands};
use commands::Host;

fn later(what: &str) -> commands::Done {
    Err(format!(
        "`{what}` is not in this build yet: it arrives with the Docker and systemd slice of v0.1.0. See docs/backlog.md"
    ))
}

fn run(cli: &Cli) -> commands::Done {
    if let Commands::Init(args) = &cli.command {
        return commands::init(args);
    }
    let (config, file) =
        config::load(cli.config.as_deref(), &config::Process).map_err(|err| err.to_string())?;
    passalong_server_core::telemetry::init(config.server.log_level);
    if let Commands::Check { health: true } = &cli.command {
        // A probe: it asks over the network and touches nothing, so whoever
        // may read the configuration may run it.
        return serve::health(&config);
    }
    let typed: Vec<String> = std::env::args().collect();
    owner::check(&config.server.data_dir, &typed.join(" "))?;
    let host = Host {
        config,
        json: cli.json,
    };
    match &cli.command {
        Commands::Init(_) => unreachable!("handled above"),
        Commands::Workspace(command) => commands::workspace(&host, command),
        Commands::Key(command) => commands::key(&host, command),
        Commands::Rewrite(command) => commands::rewrite(&host, command),
        Commands::Check { .. } => commands::check(&host, &file),
        Commands::Serve => serve::serve(host.config),
        Commands::Tls(command) => commands::tls(&host, command),
        Commands::Service { .. } => later("service"),
    }
}

fn main() -> ExitCode {
    // Usage errors exit with 2, which is clap's doing.
    let cli = Cli::parse();
    match run(&cli) {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(text) => {
            eprintln!("error: {text}");
            ExitCode::FAILURE
        }
    }
}
