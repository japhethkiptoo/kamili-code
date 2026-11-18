//! Desktop channel for Kamili: a native window (gpui-kit) for writing specs,
//! running builds and reviewing changes before they are written.
//!
//! | Engine contract | Window |
//! |---|---|
//! | events | build progress (Plan → Work → Test → Document → Write back), activity feed, test attempts |
//! | questions | the Analyst's questions in the Spec view, answered in a text box |
//! | approvals | a review panel with per-file diffs and Approve / Reject (closing the window rejects) |
//! | requests | Build, Resume, Spec buttons; Stop cancels a build at its next safe point |
//!
//! The engine runs on the CLI's tokio runtime; the window runs on GPUI's main
//! thread. [`bridge`] connects the two and [`state`] holds what is shown; both
//! are GPUI-free and tested without a window.
#![forbid(unsafe_code)]

pub mod bridge;
pub mod state;
#[cfg(feature = "ui")]
mod ui;

pub use bridge::EngineFactory;
#[cfg(feature = "ui")]
pub use ui::run;
