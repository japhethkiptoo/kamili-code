//! Model access: send a request, get text or tool calls back. Leaf module.
//!
//! Knows nothing about agents or specs. Model IDs arrive in the request.
//!
//! Two calls: [`Llm::complete`] for plain text, and [`Llm::chat`] for
//! conversations with tools. Chat messages hold the API's content blocks as raw
//! JSON, so assistant turns (including thinking blocks) are sent back exactly as
//! received, which the API requires.

#[cfg(feature = "anthropic")]
mod anthropic;
mod fake;
#[cfg(feature = "openai")]
mod openai_compat;

#[cfg(feature = "anthropic")]
pub use anthropic::{AnthropicLlm, Credential, CredentialSource};
pub use fake::{FakeLlm, FakeReply};
#[cfg(feature = "openai")]
pub use openai_compat::OpenAiCompatLlm;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub system: String,
    /// Must start with a user message and alternate roles.
    pub messages: Vec<Message>,
    pub max_tokens: u32,
    /// Reasoning effort (`low` … `max`); `None` uses the model default.
    pub effort: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LlmError {
    #[error("{0}")]
    Auth(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("API error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("model declined the request (category: {0})")]
    Refused(String),
    #[error("response was cut off at max_tokens")]
    Truncated,
    #[error("unexpected response: {0}")]
    Protocol(String),
    #[error("fake model has no scripted replies left")]
    Exhausted,
}

/// A message in a tool conversation. `content` holds API content blocks
/// (`text`, `tool_use`, `tool_result`, `thinking`, …) verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Vec<Value>,
}

impl ChatMessage {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![json!({"type": "text", "text": text.into()})],
        }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![json!({"type": "text", "text": text.into()})],
        }
    }

    /// One user message carrying the results of every tool call in a turn.
    pub fn tool_results(results: Vec<ToolResult>) -> Self {
        Self {
            role: Role::User,
            content: results
                .into_iter()
                .map(|r| {
                    json!({
                        "type": "tool_result",
                        "tool_use_id": r.tool_use_id,
                        "content": r.content,
                        "is_error": r.is_error,
                    })
                })
                .collect(),
        }
    }

    /// Text of the message: text blocks and tool-result contents, joined.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str(),
                Some("tool_result") => b["content"].as_str(),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl From<&Message> for ChatMessage {
    fn from(m: &Message) -> Self {
        Self {
            role: m.role,
            content: vec![json!({"type": "text", "text": m.content})],
        }
    }
}

/// A tool the model may call. `input_schema` is a JSON Schema object.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub system: String,
    /// Must start with a user message and alternate roles.
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub effort: Option<String>,
    /// Ask the API to cache the prompt prefix. Worth it for multi-call
    /// conversations, where each call re-sends the previous ones.
    pub cache: bool,
}

/// Token usage of one or more calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Self) {
        self.input_tokens += o.input_tokens;
        self.output_tokens += o.output_tokens;
        self.cache_read_input_tokens += o.cache_read_input_tokens;
        self.cache_creation_input_tokens += o.cache_creation_input_tokens;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StopReason {
    EndTurn,
    ToolUse,
    Other,
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// The assistant's content blocks, verbatim. Send them back unchanged.
    pub content: Vec<Value>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

impl ChatResponse {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| ToolCall {
                id: b["id"].as_str().unwrap_or_default().to_string(),
                name: b["name"].as_str().unwrap_or_default().to_string(),
                input: b["input"].clone(),
            })
            .collect()
    }

    pub fn into_message(self) -> ChatMessage {
        ChatMessage {
            role: Role::Assistant,
            content: self.content,
        }
    }
}

/// A model client. Real and fake implementations are interchangeable.
#[async_trait]
pub trait Llm: Send + Sync {
    /// Returns the concatenated text of the model's reply.
    async fn complete(&self, request: LlmRequest) -> Result<String, LlmError>;

    /// One call in a conversation that may use tools.
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError>;
}
