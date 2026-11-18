//! The unified error and its stable exit codes. Only `engine` combines leaf
//! errors into this type; channels present it but do not interpret it.

use crate::agents::AgentError;
use crate::config::ConfigError;
use crate::llm::LlmError;
use crate::sandbox::SandboxError;
use crate::spec::SpecError;

/// Stable exit codes. Never renumber; only add.
///
/// | Code | Meaning |
/// |---|---|
/// | 0 | success |
/// | 1 | workspace or I/O failure |
/// | 2 | configuration or usage error |
/// | 3 | model call failed |
/// | 4 | agent returned an invalid proposal |
/// | 5 | sandbox could not run |
/// | 6 | spec is not approved |
/// | 7 | tests still failing after all retries |
/// | 8 | rejected at an approval gate |
/// | 9 | spec missing, malformed, or edit not allowed |
/// | 10 | conflict: the real tree changed during the run |
/// | 130 | cancelled (Ctrl+C); the run can be resumed |
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("workspace: {0}")]
    Workspace(String),
    #[error("config: {0}")]
    Config(String),
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("model: {0}")]
    Llm(String),
    #[error("agent: {0}")]
    Agent(String),
    #[error("sandbox: {0}")]
    Sandbox(String),
    #[error("spec `{0}` is not approved; run `kamili spec {0}` or pass --force")]
    NotApproved(String),
    #[error("tests still failing after {attempts} attempt(s); nothing was written")]
    TestsFailed { attempts: u32 },
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("spec: {0}")]
    Spec(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("cancelled; resume with `kamili build --resume {0}`")]
    Cancelled(String),
}

impl Error {
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Workspace(_) => 1,
            Error::Config(_) | Error::Unsupported(_) => 2,
            Error::Llm(_) => 3,
            Error::Agent(_) => 4,
            Error::Sandbox(_) => 5,
            Error::NotApproved(_) => 6,
            Error::TestsFailed { .. } => 7,
            Error::Rejected(_) => 8,
            Error::Spec(_) => 9,
            Error::Conflict(_) => 10,
            Error::Cancelled(_) => 130,
        }
    }
}

impl From<ConfigError> for Error {
    fn from(e: ConfigError) -> Self {
        Error::Config(e.to_string())
    }
}

impl From<LlmError> for Error {
    fn from(e: LlmError) -> Self {
        Error::Llm(e.to_string())
    }
}

impl From<AgentError> for Error {
    fn from(e: AgentError) -> Self {
        match e {
            AgentError::Llm(e) => e.into(),
            other => Error::Agent(other.to_string()),
        }
    }
}

impl From<SandboxError> for Error {
    fn from(e: SandboxError) -> Self {
        Error::Sandbox(e.to_string())
    }
}

impl From<SpecError> for Error {
    fn from(e: SpecError) -> Self {
        match e {
            SpecError::Conflict(_) => Error::Conflict(e.to_string()),
            SpecError::Io { .. } => Error::Workspace(e.to_string()),
            _ => Error::Spec(e.to_string()),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Workspace(e.to_string())
    }
}
