//! Task planning and native orchestration.
mod journal;
pub mod plan;
pub mod run;
pub mod worktree;

// O1's scripted child remains a regression fixture.
#[cfg(test)]
mod child;
#[cfg(test)]
mod owner;
#[cfg(test)]
mod state;
#[cfg(test)]
mod tests;
