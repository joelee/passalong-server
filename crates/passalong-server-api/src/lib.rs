//! The passalong server's HTTP surface.
//!
//! Planned modules (see `docs/architecture.md` and `docs/api/`):
//!
//! - `routes`: the REST + JSON routes under `/v1`. `docs/api/openapi.json`
//!   is exported from them, and a test keeps the two identical.
//! - `content`: the routes that stream item content to and from disk.
//! - `auth`: the bearer-token layer that turns an API key into a workspace
//!   and a role before any handler runs or any body is read.
//! - `limits`: body sizes and rate limits.
//! - `tls`: rustls with certificate files, or plain HTTP behind a proxy.
//! - `health`: liveness and readiness.
//!
//! No domain rule lives here: handlers call `passalong-server-core`. No
//! response type carries a field taken from an item's `meta`.
