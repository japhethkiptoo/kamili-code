//! Runs a command against a working copy and reports what happened. Leaf module.
//!
//! Never judges whether results are good; that is the Tester's and engine's job.

mod process;

pub(crate) use process::ProcessSandbox;

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

#[derive(Debug, Clone)]
pub(crate) struct Limits {
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub(crate) struct RunOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl RunOutput {
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }

    /// stdout + stderr, keeping only the last `max_bytes` of each.
    pub fn condensed(&self, max_bytes: usize) -> String {
        format!(
            "exit code: {}{}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.exit_code
                .map_or_else(|| "none".into(), |c| c.to_string()),
            if self.timed_out { " (timed out)" } else { "" },
            tail(&self.stdout, max_bytes),
            tail(&self.stderr, max_bytes),
        )
    }
}

fn tail(s: &str, max_bytes: usize) -> &str {
    let mut start = s.len().saturating_sub(max_bytes);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SandboxError {
    #[error("empty command")]
    EmptyCommand,
    #[error("failed to start `{command}`: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },
    #[error("sandbox backend unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub(crate) trait Sandbox: Send + Sync {
    /// Whether this backend actually isolates (no network, resource limits).
    fn isolated(&self) -> bool;

    async fn run(
        &self,
        dir: &Path,
        command: &[String],
        limits: &Limits,
    ) -> Result<RunOutput, SandboxError>;
}

/// Placeholder for the isolated backend (technology still open, ARCHITECT.md §11).
pub(crate) struct ContainerSandbox;

#[async_trait]
impl Sandbox for ContainerSandbox {
    fn isolated(&self) -> bool {
        true
    }

    async fn run(&self, _: &Path, _: &[String], _: &Limits) -> Result<RunOutput, SandboxError> {
        Err(SandboxError::Unavailable(
            "the container backend is not implemented yet; set [sandbox] backend = \"process\""
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_respects_char_boundaries() {
        assert_eq!(tail("héllo", 4), "llo");
        assert_eq!(tail("abc", 10), "abc");
    }
}
