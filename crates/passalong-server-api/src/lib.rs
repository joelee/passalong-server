//! The passalong server's HTTP surface: REST + JSON over HTTP/1.1, with
//! item content as plain byte streams (`docs/api/`).
//!
//! - [`Server`]: binds, serves, and stops gracefully.
//! - `routes`: one table of the contract's operations. The router is built
//!   from it, and a test compares it with `docs/api/openapi.json`.
//! - `auth`: the bearer layer. The key is checked against the control
//!   database before a byte of the body is read, on every request.
//! - `bridge`: between asynchronous bodies and the synchronous core, in
//!   bounded pieces, so that content is never held whole.
//! - `problem`: every refusal as `application/problem+json`.
//! - [`client`]: a small HTTP/1.1 client, for the tests and for
//!   `passalong-server check --health`.
//!
//! No domain rule lives here: handlers call `passalong-server-core`, always
//! on the blocking pool, since the core is synchronous. No response type
//! carries a field taken from an item's `meta`.

mod auth;
mod bridge;
pub mod client;
mod handlers;
mod problem;
pub mod rate;
mod routes;
mod server;
mod state;
pub mod tls;

pub use bridge::BridgeStats;
pub use routes::{ROUTES, Route};
pub use server::{Options, Server, StartError};
