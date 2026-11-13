//! Narrow Linux wrappers for isolated M1 process spikes.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod auth_store;
pub mod fs;
pub mod journal;
pub mod process;
