use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

use super::{Limits, RunOutput, Sandbox, SandboxError};

/// Runs the command as a plain child process with a timeout.
///
/// Provides NO isolation: the command has network access and the user's
/// privileges. Development only; the engine warns on every run that uses it.
pub(crate) struct ProcessSandbox;

#[async_trait]
impl Sandbox for ProcessSandbox {
    fn isolated(&self) -> bool {
        false
    }

    async fn run(
        &self,
        dir: &Path,
        command: &[String],
        limits: &Limits,
    ) -> Result<RunOutput, SandboxError> {
        let (program, args) = command.split_first().ok_or(SandboxError::EmptyCommand)?;
        let child = Command::new(program)
            .args(args)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| SandboxError::Spawn {
                command: command.join(" "),
                source,
            })?;

        // Dropping the future on timeout kills the child (kill_on_drop).
        match tokio::time::timeout(limits.timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => Ok(RunOutput {
                exit_code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                timed_out: false,
            }),
            Ok(Err(source)) => Err(SandboxError::Spawn {
                command: command.join(" "),
                source,
            }),
            Err(_) => Ok(RunOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                timed_out: true,
            }),
        }
    }
}
