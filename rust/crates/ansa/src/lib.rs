//! M7 runtime and CLI. The production default remains C++ until cutover.

#![forbid(unsafe_code)]

pub mod auth;
pub mod config;
pub mod context;
pub mod frontend;
pub mod process;
pub mod protocol;
pub mod provider;
pub mod runtime;
pub mod session;
pub mod terminal_host;
pub mod tools;
pub mod transport;

// O1 exercises orchestration offline before it is wired to native sessions.
#[cfg(test)]
mod orchestration;
