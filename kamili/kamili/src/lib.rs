//! Kamili engine.
//!
//! Everything except [`api`] is crate-private. Module boundaries follow
//! ARCHITECT.md §3 and are checked by `scripts/check-module-deps.sh`:
//!
//! ```text
//! engine ──► spec | llm | sandbox | config
//!    └─────► agents ──► llm
//! api    ──► engine (+ construction types from config and llm)
//! ```
#![forbid(unsafe_code)]

mod agents;
mod api;
mod config;
mod engine;
mod index;
mod llm;
mod sandbox;
mod skills;
mod spec;

pub use api::*;
