//! Parts of the `conminer` binary that are also worth testing directly.
//!
//! The services are otherwise pure `main.rs` modules, which integration tests
//! cannot reach. The dashboard is the exception: almost everything interesting
//! about it is socket behaviour (one console connection fanned out to many
//! browsers, bytes transmitted verbatim, a vanished port noticed), and that has
//! to be exercised against a real listener rather than asserted about by
//! reading the code.

pub mod dash;
