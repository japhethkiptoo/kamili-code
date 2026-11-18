//! Claude Messages API over raw HTTP (there is no official Rust SDK).
//!
//! Credentials come from a [`CredentialSource`], asked once per request, so
//! short-lived OAuth tokens can be refreshed by the source. Where credentials
//! are stored and how a user logs in is the channel's business, not the engine's.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{ChatMessage, ChatRequest, ChatResponse, Llm, LlmError, LlmRequest, StopReason};

const API_BASE: &str = "https://api.anthropic.com/v1";
const API_VERSION: &str = "2026-10-21";
/// Required on every request authenticated with an OAuth bearer token.
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Server-side refusal fallback: on a safety-classifier refusal the API retries
/// on a suitable model instead of returning `stop_reason: "refusal"`.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const FALLBACK_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
];

/// A credential for the Claude API.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// Console API key, sent as `x-api-key`.
    ApiKey(String),
    /// OAuth access token (e.g. from `ant auth login`), sent as a bearer token.
    OAuthToken(String),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Credential::ApiKey(_) => "ApiKey(<redacted>)",
            Credential::OAuthToken(_) => "OAuthToken(<redacted>)",
        })
    }
}

/// Supplies a credential for each request. Implement this for credentials that
/// expire; a plain [`Credential`] is a source that never changes.
#[async_trait]
pub trait CredentialSource: Send + Sync {
    async fn credential(&self) -> Result<Credential, LlmError>;
}

#[async_trait]
impl CredentialSource for Credential {
    async fn credential(&self) -> Result<Credential, LlmError> {
        Ok(self.clone())
    }
}

pub struct AnthropicLlm {
    http: reqwest::Client,
    credentials: Arc<dyn CredentialSource>,
    /// `https://api.anthropic.com/v1` unless overridden (e.g. a gateway or proxy).
    base_url: String,
}

impl std::fmt::Debug for AnthropicLlm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicLlm").finish_non_exhaustive()
    }
}

impl AnthropicLlm {
    pub fn new(credentials: Arc<dyn CredentialSource>) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            credentials,
            base_url: API_BASE.to_string(),
        })
    }

    /// Sends requests to another Messages-API endpoint, such as a gateway or a
    /// proxy in front of another provider. Include the version path (`…/v1`).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        let url = base_url.into();
        let url = url.trim_end_matches('/');
        if !url.is_empty() {
            self.base_url = url.to_string();
        }
        self
    }

    pub fn with_api_key(key: impl Into<String>) -> Result<Self, LlmError> {
        Self::new(Arc::new(Credential::ApiKey(key.into())))
    }

    /// Checks the credential with a cheap, token-free call (list one model).
    pub async fn verify(&self) -> Result<(), LlmError> {
        let builder = self.http.get(format!("{}/models?limit=1", self.base_url));
        let builder = self.authorize(builder, Vec::new()).await?;
        send(builder).await.map(|_| ())
    }

    /// Adds version, auth and beta headers.
    async fn authorize(
        &self,
        builder: reqwest::RequestBuilder,
        mut betas: Vec<&'static str>,
    ) -> Result<reqwest::RequestBuilder, LlmError> {
        let builder = builder.header("anthropic-version", API_VERSION);
        let builder = match self.credentials.credential().await? {
            Credential::ApiKey(key) => builder.header("x-api-key", key),
            Credential::OAuthToken(token) => {
                betas.push(OAUTH_BETA);
                builder.bearer_auth(token)
            }
        };
        Ok(if betas.is_empty() {
            builder
        } else {
            builder.header("anthropic-beta", betas.join(","))
        })
    }
}

/// Sends a request and returns the body of a successful response.
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
            "credentials rejected (HTTP {status}); run `kamili login`"
        ))),
        code => Err(LlmError::Api {
            status: code,
            body: text,
        }),
    }
}

#[async_trait]
impl Llm for AnthropicLlm {
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
        let (body, betas) = request_body(&req);
        let builder = self
            .http
            .post(format!("{}/messages", self.base_url))
            .json(&body);
        let text = send(self.authorize(builder, betas).await?).await?;
        parse_response(&text)
    }
}

/// The Messages API body for a chat request, and the beta headers it needs.
fn request_body(req: &ChatRequest) -> (Value, Vec<&'static str>) {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .map(|m| json!({"role": m.role, "content": m.content}))
        .collect();
    let mut body = json!({
        "model": req.model,
        "max_tokens": req.max_tokens,
        "messages": messages,
    });
    if !req.system.is_empty() {
        body["system"] = json!(req.system);
    }
    if !req.tools.is_empty() {
        body["tools"] = req
            .tools
            .iter()
            .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.input_schema}))
            .collect();
    }
    if let Some(effort) = &req.effort {
        body["output_config"] = json!({ "effort": effort });
    }
    if req.cache {
        // Placed automatically on the last cacheable block, so each call in a
        // tool loop reads the previous calls' prefix from cache.
        body["cache_control"] = json!({"type": "ephemeral"});
    }
    let mut betas = Vec::new();
    if FALLBACK_MODELS.contains(&req.model.as_str()) {
        body["fallbacks"] = json!("default");
        betas.push(FALLBACK_BETA);
    }
    (body, betas)
}

fn parse_response(text: &str) -> Result<ChatResponse, LlmError> {
    let value: Value = serde_json::from_str(text).map_err(|e| LlmError::Protocol(e.to_string()))?;
    let stop_reason = match value["stop_reason"].as_str() {
        Some("refusal") => {
            let category = value["stop_details"]["category"]
                .as_str()
                .unwrap_or("unspecified");
            return Err(LlmError::Refused(category.to_string()));
        }
        Some("max_tokens") => return Err(LlmError::Truncated),
        Some("tool_use") => StopReason::ToolUse,
        Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
        _ => StopReason::Other,
    };
    let content = value["content"]
        .as_array()
        .ok_or_else(|| LlmError::Protocol("missing `content` array".into()))?
        .clone();
    let usage = serde_json::from_value(value["usage"].clone()).unwrap_or_default();
    Ok(ChatResponse {
        content,
        stop_reason,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_debug_never_shows_secret() {
        let shown = format!("{:?}", Credential::ApiKey("sk-ant-secret".into()));
        assert!(!shown.contains("secret"));
    }

    #[tokio::test]
    async fn oauth_adds_bearer_and_beta_headers() {
        let llm = AnthropicLlm::new(Arc::new(Credential::OAuthToken("tok".into()))).unwrap();
        let req = llm
            .authorize(llm.http.get("http://localhost/x"), vec![FALLBACK_BETA])
            .await
            .unwrap()
            .build()
            .unwrap();
        let h = req.headers();
        assert_eq!(h["authorization"], "Bearer tok");
        assert_eq!(
            h["anthropic-beta"],
            format!("{FALLBACK_BETA},{OAUTH_BETA}").as_str()
        );
        assert!(h.get("x-api-key").is_none());
    }

    #[test]
    fn tool_request_body_and_response() {
        let req = ChatRequest {
            model: "claude-sonnet-5-5".into(),
            system: "sys".into(),
            messages: vec![ChatMessage::user_text("hi")],
            tools: vec![super::super::ToolDef {
                name: "grep".into(),
                description: "search".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 100,
            effort: None,
            cache: true,
        };
        let (body, betas) = request_body(&req);
        assert_eq!(body["tools"][0]["name"], "grep");
        assert_eq!(body["cache_control"]["type"], "ephemeral");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(betas, [FALLBACK_BETA]);

        let resp = parse_response(
            r#"{"content":[{"type":"thinking","thinking":"","signature":"s"},{"type":"tool_use","id":"t1","name":"grep","input":{"pattern":"x"}}],
                "stop_reason":"tool_use","usage":{"input_tokens":5,"output_tokens":2,"cache_read_input_tokens":3}}"#,
        )
        .unwrap();
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.tool_calls()[0].input["pattern"], "x");
        assert_eq!(resp.usage.cache_read_input_tokens, 3);
        // Thinking blocks are kept verbatim for the next call.
        assert_eq!(resp.into_message().content[0]["signature"], "s");
        assert!(matches!(
            parse_response(r#"{"content":[],"stop_reason":"max_tokens"}"#),
            Err(LlmError::Truncated)
        ));
    }

    #[test]
    fn base_url_override() {
        let llm = AnthropicLlm::with_api_key("k").unwrap();
        assert_eq!(llm.base_url, API_BASE);
        let llm = llm.with_base_url("http://localhost:4000/v1/");
        assert_eq!(llm.base_url, "http://localhost:4000/v1");
        assert_eq!(llm.with_base_url("").base_url, "http://localhost:4000/v1");
    }

    #[tokio::test]
    async fn api_key_uses_x_api_key_only() {
        let llm = AnthropicLlm::with_api_key("sk-ant-k").unwrap();
        let req = llm
            .authorize(llm.http.get("http://localhost/x"), Vec::new())
            .await
            .unwrap()
            .build()
            .unwrap();
        let h = req.headers();
        assert_eq!(h["x-api-key"], "sk-ant-k");
        assert_eq!(h["anthropic-version"], API_VERSION);
        assert!(h.get("authorization").is_none() && h.get("anthropic-beta").is_none());
    }
}
