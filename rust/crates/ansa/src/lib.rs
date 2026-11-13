//! M3 auth migration. Production runtime remains in the C++ executable.

#![forbid(unsafe_code)]

pub mod auth;
pub mod process;
pub mod protocol;
pub mod provider;
pub mod tools;
pub mod transport;

/// The scaffold must not present itself as an implemented agent runtime.
pub const SCAFFOLD_MESSAGE: &str =
    "ansa-rs: M3 protocol/auth port; Rust runtime is not implemented. Use the C++ ansa executable.";
