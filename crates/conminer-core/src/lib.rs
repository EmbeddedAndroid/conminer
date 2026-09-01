//! conminer core — the importable library (§2: "small, importable core library,
//! so uart-mcp (or anyone) can vendor the miner").
//!
//! Pipeline, in order:
//!
//! ```text
//! bytes → linesplit::LineSplitter → framer::Framer (records)
//!       → drain::Drain (templates) → store::Store (SQLite, WAL)
//! ```
//!
//! The invariant that holds across all of it (§6): **raw bytes are never
//! rewritten**. Templates, records, stages, search indexes and fingerprints are
//! all derived views that can be rebuilt from the raw store at any time.

pub mod absence;
pub mod bisect;
pub mod broker;
pub mod clock;
pub mod codec;
pub mod config;
pub mod console;
pub mod decode;
pub mod discovery;
pub mod drain;
pub mod error;
pub mod follow;
pub mod framer;
pub mod ftdi;
pub mod hooks;
pub mod ingest;
pub mod linesplit;
pub mod live;
pub mod peers;
pub mod pipeline;
pub mod recovery;
pub mod reports;
pub mod runner;
pub mod search;
pub mod store;
pub mod symbolize;
pub mod tac;

/// Which SOURCE this binary was built from.
///
/// Not the Cargo version. Three nodes once reported `0.2.0` while running three
/// genuinely different builds -- a field that always agrees can never disagree
/// when it matters, and in a fleet where one node proxies tool calls to another,
/// a behaviour difference between builds arrives looking like a misbehaving
/// board rather than a deployment problem.
///
/// Baked in by the build (`--build-arg CONMINER_BUILD=...`, computed by
/// `./cm build-id`) rather than derived from the binary, so the same source
/// yields the same string on every architecture: the lab hosts are x86_64 and
/// the dev box is arm64, and a binary hash would report those as different
/// deployments of identical code.
pub fn build_id() -> &'static str {
    option_env!("CONMINER_BUILD").unwrap_or("unknown")
}
pub mod target;
pub mod transfer;
pub mod usb;
pub mod values;

pub use error::{ErrorCode, Result, ToolError};

pub use drain::strip_ansi;
