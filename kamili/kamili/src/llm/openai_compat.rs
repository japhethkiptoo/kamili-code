//! OpenAI-compatible Chat Completions over raw HTTP: NVIDIA NIM
//! (build.nvidia.com), OpenRouter, Groq, Ollama, vLLM, LM Studio and others.
//!
//! The rest of Kamili speaks in Anthropic-style content blocks (`text`,
//! `tool_use`, `tool_result`, `thinking`). This client translates them to
//! Chat Completions messages on the way out, and the reply back into blocks on
//! the way in, so agents and the engine are unchanged.
//!
//! Not supported by this API, so dropped: reasoning effort, prompt caching
//! hints and thinking blocks.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use super::{
    ChatMessage, ChatRequest, ChatResponse, Llm, LlmError, LlmRequest, Role, StopReason, Usage,
};

pub struct OpenAiCompatLlm {
    http: reqwest::Client,
    /// Without a trailing slash, e.g. `https://integrate.api.nvidia.com/v1`.
    base_url: String,
    /// Sent as a bearer token. `None` for local servers that need no key.
    api_key: Option<String>,
}

impl std::fmt::Debug for OpenAiCompatLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatLlm")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatLlm {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Result<Self, LlmError> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        if base_url.is_empty() {
            return Err(LlmError::Auth("no base URL for the model provider".into()));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            base_url,
            api_key: api_key.filter(|k| !k.is_empty()),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Checks the endpoint and key by listing models (no tokens used).
    pub async fn verify(&self) -> Result<(), LlmError> {
        let builder = self.http.get(format!("{}/models", self.base_url));
        send(self.authorize(builder)).await.map(|_| ())
    }

    fn authorize(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(key) => builder.bearer_auth(key),
            None => builder,
        }
    }
}

async fn send(builder: reqwest::RequestBuilder) -> Result<String, LlmError> {
    let resp = builder
        .send()
        .await
        .map_err(|e| LlmError::Transport(e.to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| LlmError::Transport(e.to_string()))?;
    match status.as_u16() {
        200..=299 => Ok(text),
        401 | 403 => Err(LlmError::Auth(format!(
            "credentials rejected by the model provider (HTTP {status}); check the API key in [provider] api_key_env"
        ))),
        code => Err(LlmError::Api {
            status: code,
            body: text,
        }),
    }
}

#[async_trait]
impl Llm for OpenAiCompatLlm {
    async fn complete(&self, req: LlmRequest) -> Result<String, LlmError> {
        let chat = ChatRequest {
            messages: req.messages.iter().map(ChatMessage::from).collect(),
            model: req.model,
            system: req.system,
            tools: Vec::new(),
            max_tokens: req.max_tokens,
            effort: req.effort,
            cache: false,
        };
        Ok(self.chat(chat).await?.text())
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, LlmError> {
        let body = request_body(&req);
        let builder = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);
        let text = send(self.authorize(builder)).await?;
        parse_response(&text)
    }
}

/// The Chat Completions body for a chat request.
fn request_body(req: &ChatRequest) -> Value {
    let mut messages = Vec::new();
    if !req.system.is_empty() {
        messages.push(json!({"role": "system", "content": req.system}));
    }
    for m in &req.messages {
        match m.role {
            Role::User => push_user(&mut messages, m),
            Role::Assistant => messages.push(assistant_message(m)),
        }
    }
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "messages": messages,
    });
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    },
                })
            })
            .collect();
    }
    body
}

/// A user turn: each `tool_result` becomes a `tool` message (they must come
/// straight after the assistant's tool calls), then any text as a user message.
fn push_user(out: &mut Vec<Value>, m: &ChatMessage) {
    let mut text = Vec::new();
    for block in &m.content {
        match block["type"].as_str() {
            Some("tool_result") => {
                let content = tool_result_text(&block["content"]);
                let content = if block["is_error"].as_bool() == Some(true) {
                    format!("error: {content}")
                } else {
                    content
                };
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": block["tool_use_id"],
                    "content": content,
                }));
            }
            Some("text") => text.extend(block["text"].as_str().map(str::to_string)),
            _ => {}
        }
    }
    if !text.is_empty() {
        out.push(json!({"role": "user", "content": text.join("\n")}));
    }
}

/// `tool_result.content` is a string, or a list of text blocks.
fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// An assistant turn: text blocks become `content`, `tool_use` blocks become
/// `tool_calls`. Thinking blocks are Anthropic-only and are dropped.
fn assistant_message(m: &ChatMessage) -> Value {
    let mut text = Vec::new();
    let mut calls = Vec::new();
    for block in &m.content {
        match block["type"].as_str() {
            Some("text") => text.extend(block["text"].as_str().map(str::to_string)),
            Some("tool_use") => calls.push(json!({
                "id": block["id"],
                "type": "function",
                "function": {
                    "name": block["name"],
                    "arguments": block["input"].to_string(),
                },
            })),
            _ => {}
        }
    }
    let mut msg = json!({"role": "assistant"});
    msg["content"] = if text.is_empty() {
        Value::Null
    } else {
        json!(text.join("\n"))
    };
    if !calls.is_empty() {
        msg["tool_calls"] = Value::Array(calls);
    }
    msg
}

/// Turns a Chat Completions reply into Anthropic-style content blocks.
fn parse_response(text: &str) -> Result<ChatResponse, LlmError> {
    let value: Value = serde_json::from_str(text).map_err(|e| LlmError::Protocol(e.to_string()))?;
    let choice = value["choices"]
        .get(0)
        .ok_or_else(|| LlmError::Protocol("missing `choices[0]`".into()))?;
    let message = &choice["message"];

    let mut content = Vec::new();
    if let Some(t) = message["content"].as_str().filter(|t| !t.is_empty()) {
        content.push(json!({"type": "text", "text": t}));
    }
    let calls = message["tool_calls"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for (i, call) in calls.iter().enumerate() {
        let id = call["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("call_{i}"));
        let input = parse_arguments(&call["function"]["arguments"]);
        content.push(json!({
            "type": "tool_use",
            "id": id,
            "name": call["function"]["name"],
            "input": input,
        }));
    }

    let stop_reason = match choice["finish_reason"].as_str() {
        Some("length") => return Err(LlmError::Truncated),
        Some("content_filter") => return Err(LlmError::Refused("content_filter".into())),
        _ if !calls.is_empty() => StopReason::ToolUse,
        Some("stop") | Some("tool_calls") | None => StopReason::EndTurn,
        _ => StopReason::Other,
    };

    let u = &value["usage"];
    let usage = Usage {
        input_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
        output_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
        cache_read_input_tokens: u["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0),
        cache_creation_input_tokens: 0,
    };
    Ok(ChatResponse {
        content,
        stop_reason,
        usage,
    })
}

/// Arguments arrive as a JSON string (some servers send an object). Malformed
/// arguments become an empty object, so the tool reports the missing fields to
/// the model, which can retry, instead of failing the whole run.
fn parse_arguments(args: &Value) -> Value {
    match args {
        Value::Object(_) => args.clone(),
        Value::String(s) if s.trim().is_empty() => Value::Object(Map::new()),
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(v @ Value::Object(_)) => v,
            _ => Value::Object(Map::new()),
        },
        _ => Value::Object(Map::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ToolDef, ToolResult};
    use super::*;

    fn tool_request() -> ChatRequest {
        let assistant = ChatMessage {
            role: Role::Assistant,
            content: vec![
                json!({"type": "thinking", "thinking": "hmm", "signature": "s"}),
                json!({"type": "text", "text": "Looking."}),
                json!({"type": "tool_use", "id": "t1", "name": "grep", "input": {"pattern": "x"}}),
            ],
        };
        ChatRequest {
            model: "qwen/qwen3-coder".into(),
            system: "sys".into(),
            messages: vec![
                ChatMessage::user_text("hi"),
                assistant,
                ChatMessage::tool_results(vec![ToolResult {
                    tool_use_id: "t1".into(),
                    content: "no match".into(),
                    is_error: true,
                }]),
            ],
            tools: vec![ToolDef {
                name: "grep".into(),
                description: "search".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 100,
            effort: Some("high".into()),
            cache: true,
        }
    }

    #[test]
    fn request_translates_blocks_to_chat_messages() {
        let body = request_body(&tool_request());
        let m = body["messages"].as_array().unwrap();
        assert_eq!(m.len(), 4);
        assert_eq!(m[0], json!({"role": "system", "content": "sys"}));
        assert_eq!(m[1], json!({"role": "user", "content": "hi"}));
        // Thinking dropped; text kept; tool call arguments are a JSON string.
        assert_eq!(m[2]["content"], "Looking.");
        assert_eq!(m[2]["tool_calls"][0]["id"], "t1");
        assert_eq!(m[2]["tool_calls"][0]["function"]["name"], "grep");
        assert_eq!(
            m[2]["tool_calls"][0]["function"]["arguments"],
            r#"{"pattern":"x"}"#
        );
        assert_eq!(m[3]["role"], "tool");
        assert_eq!(m[3]["tool_call_id"], "t1");
        assert_eq!(m[3]["content"], "error: no match");

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
        // Anthropic-only options are not sent.
        assert!(body.get("output_config").is_none() && body.get("cache_control").is_none());
    }

    #[test]
    fn assistant_with_only_tool_calls_has_null_content() {
        let m = assistant_message(&ChatMessage {
            role: Role::Assistant,
            content: vec![json!({"type": "tool_use", "id": "a", "name": "read", "input": {}})],
        });
        assert!(m["content"].is_null());
        assert_eq!(m["tool_calls"][0]["function"]["arguments"], "{}");
    }

    #[test]
    fn response_becomes_content_blocks() {
        let resp = parse_response(
            r#"{"choices":[{"finish_reason":"tool_calls","message":{"role":"assistant","content":"ok",
                "tool_calls":[{"id":"c1","type":"function","function":{"name":"grep","arguments":"{\"pattern\":\"y\"}"}},
                              {"type":"function","function":{"name":"read","arguments":"not json"}}]}}],
                "usage":{"prompt_tokens":7,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":2}}}"#,
        )
        .unwrap();
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.text(), "ok");
        let calls = resp.tool_calls();
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[0].input["pattern"], "y");
        // Missing id is generated; malformed arguments become `{}`.
        assert_eq!(calls[1].id, "call_1");
        assert_eq!(calls[1].input, json!({}));
        assert_eq!(resp.usage.input_tokens, 7);
        assert_eq!(resp.usage.output_tokens, 3);
        assert_eq!(resp.usage.cache_read_input_tokens, 2);
    }

    #[test]
    fn round_trip_through_engine_message() {
        // A parsed reply, stored as an assistant message, translates back.
        let resp = parse_response(
            r#"{"choices":[{"finish_reason":"tool_calls","message":{"content":null,
                "tool_calls":[{"id":"c9","type":"function","function":{"name":"list","arguments":"{\"dir\":\"src\"}"}}]}}]}"#,
        )
        .unwrap();
        let back = assistant_message(&resp.into_message());
        assert_eq!(back["tool_calls"][0]["id"], "c9");
        assert_eq!(
            back["tool_calls"][0]["function"]["arguments"],
            r#"{"dir":"src"}"#
        );
    }

    #[test]
    fn finish_reasons() {
        let r = |reason: &str| {
            parse_response(&format!(
                r#"{{"choices":[{{"finish_reason":"{reason}","message":{{"content":"x"}}}}]}}"#
            ))
        };
        assert_eq!(r("stop").unwrap().stop_reason, StopReason::EndTurn);
        assert!(matches!(r("length"), Err(LlmError::Truncated)));
        assert!(matches!(r("content_filter"), Err(LlmError::Refused(_))));
        assert!(matches!(
            parse_response(r#"{"choices":[]}"#),
            Err(LlmError::Protocol(_))
        ));
    }

    #[test]
    fn base_url_is_normalised_and_required() {
        let llm = OpenAiCompatLlm::new("https://integrate.api.nvidia.com/v1/", None).unwrap();
        assert_eq!(llm.base_url(), "https://integrate.api.nvidia.com/v1");
        assert!(OpenAiCompatLlm::new("", None).is_err());
    }

    #[test]
    fn bearer_only_when_key_given() {
        let with = OpenAiCompatLlm::new("http://localhost/v1", Some("nvapi-k".into())).unwrap();
        let req = with
            .authorize(with.http.get("http://localhost/x"))
            .build()
            .unwrap();
        assert_eq!(req.headers()["authorization"], "Bearer nvapi-k");
        let without = OpenAiCompatLlm::new("http://localhost/v1", Some(String::new())).unwrap();
        let req = without
            .authorize(without.http.get("http://localhost/x"))
            .build()
            .unwrap();
        assert!(req.headers().get("authorization").is_none());
    }
}
