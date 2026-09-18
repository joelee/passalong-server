//! The passalong server's HTTP surface.
//!
//! Planned modules (see `docs/architecture.md` and `docs/api/`):
//!
//! - `schema`: the GraphQL schema: queries, mutations, and subscriptions.
//!   `docs/api/schema.graphql` is exported from it, and a test keeps the
//!   two identical.
//! - `content`: the plain HTTP endpoints that stream item content, which
//!   GraphQL is not suited to.
//! - `auth`: the bearer-token layer that turns an API key into a workspace
//!   and a role before any resolver runs.
//! - `limits`: body sizes, query depth and complexity, and rate limits.
//! - `tls`: rustls with certificate files, or plain HTTP behind a proxy.
//! - `health`: liveness and readiness.
//!
//! No domain rule lives here: resolvers call `passalong-server-core`.
