//! M1 migration risk spikes. Production behavior remains in the C++ executable.

#![forbid(unsafe_code)]

pub mod process;
pub mod protocol;
pub mod transport;

/// The scaffold must not present itself as an implemented agent runtime.
pub const SCAFFOLD_MESSAGE: &str =
    "ansa-rs: M1 risk spikes; Rust runtime is not implemented. Use the C++ ansa executable.";
