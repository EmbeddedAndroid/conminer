//! conminer MCP server (§8).
//!
//! A thin layer over the core library: it owns the wire protocol, the tool
//! schemas, and the discipline that makes the surface safe for an agent — hard
//! caps, cursors, structured errors, and a freshness envelope on every read.

pub mod handler;
pub mod notify;
pub mod protocol;
pub mod push;
pub mod report;
pub mod route;
pub mod selftest;
pub mod server;
pub mod state;
pub mod tools;

pub use handler::Handler;
pub use notify::Broadcaster;
pub use server::Server;
pub use state::Context;
