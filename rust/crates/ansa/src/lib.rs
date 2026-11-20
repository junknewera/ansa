//! M7 runtime and CLI. The production default remains C++ until cutover.

#![forbid(unsafe_code)]

pub mod auth;
pub mod config;
pub mod context;
pub mod frontend;
mod harness;
mod mcp;
#[path = "provider/openrouter.rs"]
mod openrouter;
pub mod orchestration;
pub mod process;
pub mod protocol;
pub mod provider;
pub mod runtime;
pub mod session;
pub mod terminal_host;
pub mod tools;
pub mod transport;
