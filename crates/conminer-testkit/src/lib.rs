//! Shared test infrastructure (§12).
//!
//! Every crate's tests build on this so the six layers of the harness stay
//! consistent: the same corpus loader, the same pipeline construction, the same
//! fault-injection vocabulary. A test that needs its own bespoke rig is usually a
//! sign the production API is awkward.

pub mod corpus;
pub mod fault;
pub mod frame;
pub mod mcp;
pub mod pty;
pub mod rig;

pub use corpus::{corpus_dir, corpus_file, corpus_files};
pub use frame::{frame_text, FrameResult};
pub use mcp::McpRig;
pub use rig::Rig;
