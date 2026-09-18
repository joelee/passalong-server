//! The passalong server's domain, free of HTTP and terminal code.
//!
//! What exists: the rules of one workspace (PLAN-00001), and the stores
//! under them (PLAN-00002).
//!
//! - [`workspace`]: the [`workspace::Engine`] and the [`workspace::Rules`]
//!   it runs, one ledger transaction per API operation; the encryption
//!   state and the changes that re-encrypt nothing.
//! - [`upload`]: the upload lifecycle, with its replays and the janitor.
//! - [`rewrite`]: the lease-held rewrite session behind `passalong encrypt`.
//! - [`shelf`]: where items are kept: in memory, or on a filesystem.
//! - [`ledger`]: what a workspace remembers between requests: in memory, or
//!   in SQLite. Its transaction is the workspace lock, across processes.
//! - [`ids`], [`error`]: strict identifiers; the API's error codes.
//! - [`clock`], [`random`]: time and randomness, injected.
//! - [`fault`]: test support, empty in the server's build.
//!
//! Planned for the next slices of v0.1.0 (see `docs/backlog.md`): `config`,
//! `auth` (API keys), the CLI's commands, and `telemetry`. The stores log
//! through `tracing`, with workspace, item, and upload ids, sizes, and
//! outcomes, and never `meta`, content, or a header (`tests/logs.rs`).
//!
//! Both the API crate and the CLI depend on this crate, so every rule about
//! keys and workspaces lives here once, whether it is reached over HTTPS or
//! from `passalong-server key create`.

pub mod clock;
pub mod error;
pub mod fault;
pub mod ids;
pub mod ledger;
pub mod random;
pub mod rewrite;
pub mod shelf;
pub mod upload;
pub mod workspace;
