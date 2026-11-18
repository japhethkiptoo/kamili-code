//! The only public surface of the engine (ARCHITECT.md §7). No logic lives here.
//!
//! Additive changes only: removing or renaming anything below is a breaking change.

// The Channel contract.
pub use crate::engine::{
    AgentName, ApprovalKind, ApprovalRequest, Channel, Decision, Event, FileChangePreview,
    LogLevel, Question, RunStep, Stage,
};

// Entry points and their request/outcome types.
pub use crate::engine::{BuildOutcome, BuildRequest, Engine, SpecOutcome, SpecRequest};

// Build runs: listing, cleanup, and cancelling a running build.
pub use crate::engine::{CancelToken, RunSummary, clean_runs, discard_run, list_runs};

// Errors and exit codes.
pub use crate::engine::Error;

// The code graph agents navigate with (`kamili graph`).
pub use crate::engine::{GraphQuery, explore_graph};

// Required agent skills (`[skills]` in config): what is installed and what is missing.
pub use crate::engine::{SkillStatus, check_skills};

// Construction: configuration and model clients. Channels act as composition
// roots, so they need these to build an `Engine`.
pub use crate::config::{
    BuildConfig, Config, ConfigError, ContextConfig, ModelConfig, ProviderConfig, ProviderKind,
    RequiredSkills, SandboxBackend, SandboxConfig, SessionConfig, SkillsConfig,
    load as load_config,
};
#[cfg(feature = "openai")]
pub use crate::llm::OpenAiCompatLlm;
#[cfg(feature = "anthropic")]
pub use crate::llm::{AnthropicLlm, Credential, CredentialSource};
pub use crate::llm::{FakeLlm, FakeReply, Llm, LlmError, LlmRequest, Message, Role};
// Tool conversations (`Llm::chat`), for implementing a model client.
pub use crate::llm::{
    ChatMessage, ChatRequest, ChatResponse, StopReason, ToolCall, ToolDef, ToolResult, Usage,
};
