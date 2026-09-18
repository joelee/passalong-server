//! The passalong server's domain, free of HTTP and terminal code.
//!
//! Planned modules (see `docs/architecture.md`):
//!
//! - `config`: `config.toml` lookup, parsing, and validation.
//! - `workspace`: workspaces, their quotas, and their encryption state.
//! - `auth`: API keys: generation, hashing, expiry, and verification.
//! - `items`: the item store of one workspace: staged uploads, atomic
//!   publish, deduplication, listing, id resolution, and deletion.
//! - `rewrite`: the lease-held rewrite session behind `passalong encrypt`.
//! - `control`: the control-plane database the server and the CLI share.
//! - `telemetry`: logging with the levels `AGENTS.md` names.
//!
//! Both the API crate and the CLI depend on this crate, so every rule about
//! keys and workspaces lives here once, whether it is reached over HTTPS or
//! from `passalong-server key create`.
