//! M0 migration scaffold. Production behavior remains in the C++ executable.

#![forbid(unsafe_code)]

/// The scaffold must not present itself as an implemented agent runtime.
pub const SCAFFOLD_MESSAGE: &str =
    "ansa-rs: M0 scaffold; Rust runtime is not implemented. Use the C++ ansa executable.";
