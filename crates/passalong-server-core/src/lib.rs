//! The passalong server's domain, free of HTTP and terminal code.
//!
//! What exists (PLAN-00001, the protocol spike): the rules of one
//! workspace, over any [`shelf::ItemShelf`], with no I/O of their own.
//!
//! - [`ids`]: identifiers, parsed strictly.
//! - [`error`]: the API's error codes.
//! - [`workspace`]: the [`workspace::Engine`], its encryption state, and
//!   the changes that re-encrypt nothing.
//! - [`upload`]: the upload lifecycle, with its replays.
//! - [`rewrite`]: the lease-held rewrite session behind `passalong encrypt`.
//! - [`shelf`]: where items are kept; in memory for now.
//! - [`clock`], [`random`]: time and randomness, injected.
//!
//! Planned for v0.1.0 (see `docs/architecture.md`): `config`, `auth` (API
//! keys), `control` (the database the server and the CLI share), the
//! filesystem shelf, and `telemetry`. The rules here log nothing yet,
//! having no I/O; the routes that call them will log key ids, item ids,
//! sizes, and outcomes, and never `meta`, content, or a header.
//!
//! Both the API crate and the CLI depend on this crate, so every rule about
//! keys and workspaces lives here once, whether it is reached over HTTPS or
//! from `passalong-server key create`.

pub mod clock;
pub mod error;
pub mod ids;
pub mod random;
pub mod rewrite;
pub mod shelf;
pub mod upload;
pub mod workspace;
