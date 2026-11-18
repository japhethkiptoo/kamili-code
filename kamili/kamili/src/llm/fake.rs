use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    ChatMessage, ChatRequest, ChatResponse, Llm, LlmError, LlmRequest, Message, StopReason, Usage,
};

/// One scripted model reply.
#[derive(Debug, Clone, PartialEq)]
pub enum FakeReply {
    /// A final text reply.
    Text(String),
    /// A turn that calls these tools: `(name, input)`.
    Tools(Vec<(String, Value)>),
}

impl FakeReply {
    /// A turn calling one tool.
    pub fn tool(name: &str, input: Value) -> Self {
        FakeReply::Tools(vec![(name.to_string(), input)])
    }
}

impl From<&str> for FakeReply {
    fn from(s: &str) -> Self {
        FakeReply::Text(s.to_string())
    }
}

impl From<String> for FakeReply {
    fn from(s: String) -> Self {
        FakeReply::Text(s)
    }
}

/// Replays canned replies in order and records every request it receives.
///
/// Every call (`complete` or `chat`) takes the next reply. When none are left
/// it fails with [`LlmError::Exhausted`], so a test notices extra calls.
#[derive(Debug, Default)]
pub struct FakeLlm {
    replies: Mutex<VecDeque<FakeReply>>,
    seen: Mutex<Vec<ChatRequest>>,
    calls: Mutex<u64>,
}

impl FakeLlm {
    pub fn new<I, S>(replies: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<FakeReply>,
    {
        Self {
            replies: Mutex::new(replies.into_iter().map(Into::into).collect()),
            ..Default::default()
        }
    }

    /// Requests received so far, flattened to text (tool results included).
    pub fn requests(&self) -> Vec<LlmRequest> {
        self.chat_requests()
            .into_iter()
            .map(|r| LlmRequest {
                model: r.model,
                system: r.system,
                messages: r
                    .messages
                    .iter()
                    .map(|m| Message {
                        role: m.role,
                        content: m.text(),
                    })
                    .collect(),
                max_tokens: r.max_tokens,
                effort: r.effort,
            })
            .collect()
    }

    /// Requests received so far, with tools and raw content blocks.
    pub fn chat_requests(&self) -> Vec<ChatRequest> {
        self.seen.lock().expect("fake llm lock").clone()
    }

    /// Replies not yet consumed.
    pub fn remaining(&self) -> usize {
        self.replies.lock().expect("fake llm lock").len()
    }

    fn next(&self, request: ChatRequest) -> Result<FakeReply, LlmError> {
        self.seen.lock().expect("fake llm lock").push(request);
        self.replies
            .lock()
            .expect("fake llm lock")
            .pop_front()
            .ok_or(LlmError::Exhausted)
    }
}

#[async_trait]
impl Llm for FakeLlm {
    async fn complete(&self, request: LlmRequest) -> Result<String, LlmError> {
        let chat = ChatRequest {
            messages: request.messages.iter().map(ChatMessage::from).collect(),
            model: request.model,
            system: request.system,
            tools: Vec::new(),
            max_tokens: request.max_tokens,
            effort: request.effort,
            cache: false,
        };
        match self.next(chat)? {
            FakeReply::Text(t) => Ok(t),
            FakeReply::Tools(_) => Err(LlmError::Protocol(
                "scripted a tool call for a call without tools".into(),
            )),
        }
    }

    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 10,
            ..Usage::default()
        };
        match self.next(request)? {
            FakeReply::Text(text) => Ok(ChatResponse {
                content: vec![json!({"type": "text", "text": text})],
                stop_reason: StopReason::EndTurn,
                usage,
            }),
            FakeReply::Tools(calls) => {
                let content = calls
                    .into_iter()
                    .map(|(name, input)| {
                        let mut n = self.calls.lock().expect("fake llm lock");
                        *n += 1;
                        json!({"type": "tool_use", "id": format!("toolu_fake_{n}"), "name": name, "input": input})
                    })
                    .collect();
                Ok(ChatResponse {
                    content,
                    stop_reason: StopReason::ToolUse,
                    usage,
                })
            }
        }
    }
}
