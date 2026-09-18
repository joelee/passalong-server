//! The passalong server's domain, free of HTTP and terminal code.
//!
//! What exists: the rules of one workspace (PLAN-00001), the stores under
//! them (PLAN-00002), and who may use them (PLAN-00003).
//!
//! - [`workspace`]: the [`workspace::Engine`] and the [`workspace::Rules`]
//!   it runs, one ledger transaction per API operation; the encryption
//!   state and the changes that re-encrypt nothing.
//! - [`upload`]: the upload lifecycle, with its replays and the janitor.
//! - [`rewrite`]: the lease-held rewrite session behind `passalong encrypt`,
//!   and the operator's abort of one nobody will finish.
//! - [`shelf`]: where items are kept: in memory, or on a filesystem.
//! - [`ledger`]: what a workspace remembers between requests: in memory, or
//!   in SQLite. Its transaction is the workspace lock, across processes.
//! - [`auth`]: API keys: made, parsed, hashed, compared; shown once.
//! - [`control`]: workspaces and keys as the operator manages them, the
//!   audit trail, and [`control::Control::authenticate`], which the HTTP
//!   layer will call on every request. The database's schema, as the steps
//!   that made it.
//! - [`config`]: `config.toml`: where it is found and what it may hold.
//! - [`telemetry`]: logging, with only an allow-list of fields written.
//! - [`ids`], [`error`]: strict identifiers; the API's error codes.
//! - [`clock`], [`random`]: time and randomness, injected.
//! - [`fault`]: test support, empty in the server's build.
//!
//! Both the API crate and the CLI depend on this crate, so every rule about
//! keys and workspaces lives here once, whether it is reached over HTTPS or
//! from `passalong-server key create`.

pub mod auth;
pub mod clock;
pub mod config;
pub mod control;
pub mod error;
pub mod fault;
pub mod ids;
pub mod ledger;
pub mod random;
pub mod rewrite;
pub mod shelf;
pub mod telemetry;
pub mod upload;
pub mod workspace;
