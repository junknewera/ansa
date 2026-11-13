//! M5 offline runtime library. The production entry point remains C++.

#![forbid(unsafe_code)]

pub mod auth;
pub mod context;
pub mod frontend;
pub mod process;
pub mod protocol;
pub mod provider;
pub mod runtime;
pub mod session;
pub mod tools;
pub mod transport;

/// The scaffold must not present itself as an implemented agent runtime.
pub const SCAFFOLD_MESSAGE: &str = "ansa-rs: M5 offline runtime library; Rust CLI is not implemented. Use the C++ ansa executable.";
