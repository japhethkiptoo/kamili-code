//! Agent roles: turn role + context into a prompt, and the model's reply into a
//! structured proposal. Uses only `llm`.
//!
//! Agents never apply anything or talk to the user. Some may call tools (look
//! up code, and for the Worker edit the working copy); the engine runs those
//! calls through a [`ToolHost`] and decides what each is allowed to touch.
//! Prompts are reviewable markdown in `prompts/`.

mod tools;

pub(crate) use tools::{ToolHost, ToolKit, ToolRequest};

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::llm::{ChatMessage, ChatRequest, Llm, LlmError, Message, ToolResult, Usage};

const ANALYST_PROMPT: &str = include_str!("prompts/analyst.md");
const ARCHITECT_PROMPT: &str = include_str!("prompts/architect.md");
const PLANNER_PROMPT: &str = include_str!("prompts/planner.md");
const WORKER_PROMPT: &str = include_str!("prompts/worker.md");
const TESTER_PROMPT: &str = include_str!("prompts/tester.md");
const DOCUMENTOR_PROMPT: &str = include_str!("prompts/documentor.md");

const MAX_TOKENS: u32 = 16_000;

/// The agent roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum AgentRole {
    Analyst,
    Architect,
    Planner,
    Worker,
    Tester,
    Documentor,
}

impl AgentRole {
    pub const ALL: [AgentRole; 6] = [
        AgentRole::Analyst,
        AgentRole::Architect,
        AgentRole::Planner,
        AgentRole::Worker,
        AgentRole::Tester,
        AgentRole::Documentor,
    ];

    pub fn name(self) -> &'static str {
        match self {
            AgentRole::Analyst => "Analyst",
            AgentRole::Architect => "Architect",
            AgentRole::Planner => "Planner",
            AgentRole::Worker => "Worker",
            AgentRole::Tester => "Tester",
            AgentRole::Documentor => "Documentor",
        }
    }
}

/// Model ID per role. Filled in by the engine from config.
#[derive(Debug, Clone)]
pub(crate) struct AgentModels {
    pub analyst: String,
    pub architect: String,
    pub planner: String,
    pub worker: String,
    pub tester: String,
    pub documentor: String,
    pub effort: Option<String>,
}

/// Per-turn tool budgets (`[context]` in config).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolLimits {
    pub max_calls: u32,
    pub max_bytes: usize,
}

/// Usage of one role across a run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RoleUsage {
    pub calls: u32,
    pub tool_calls: u32,
    #[serde(flatten)]
    pub tokens: Usage,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AgentError {
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error("{agent} returned malformed output: {reason}")]
    Malformed { agent: &'static str, reason: String },
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SectionEditProposal {
    pub section: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PlanProposal {
    pub summary: String,
    #[serde(default)]
    pub section_edits: Vec<SectionEditProposal>,
    /// What the Worker needs to know: relevant files, symbols, conventions.
    #[serde(default)]
    pub context_brief: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FileChange {
    /// Path relative to the project root.
    pub path: String,
    /// New full contents; `None` deletes the file.
    pub content: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WorkProposal {
    pub summary: String,
    /// Whole-file changes. Optional: the Worker normally edits with tools.
    #[serde(default)]
    pub files: Vec<FileChange>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TestFeedback {
    pub feedback: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DesignProposal {
    pub summary: String,
    #[serde(default)]
    pub section_edits: Vec<SectionEditProposal>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DocProposal {
    pub summary: String,
    #[serde(default)]
    pub section_edits: Vec<SectionEditProposal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum AnalystTurn {
    Question { text: String },
    Draft { spec: String },
}

pub(crate) struct Agents {
    llm: Arc<dyn Llm>,
    models: AgentModels,
    /// Text appended to a role's system prompt (its required skills).
    skills: HashMap<AgentRole, String>,
    limits: ToolLimits,
    usage: Mutex<BTreeMap<AgentRole, RoleUsage>>,
}

/// What a tool-using agent gets to see and do.
pub(crate) struct Workspace<'a> {
    pub repo_map: &'a str,
    pub host: &'a mut dyn ToolHost,
}

impl Agents {
    pub fn new(
        llm: Arc<dyn Llm>,
        models: AgentModels,
        skills: HashMap<AgentRole, String>,
        limits: ToolLimits,
    ) -> Self {
        Self {
            llm,
            models,
            skills,
            limits,
            usage: Mutex::new(BTreeMap::new()),
        }
    }

    /// Usage per role since the last call, and resets it.
    pub fn take_usage(&self) -> BTreeMap<AgentRole, RoleUsage> {
        std::mem::take(&mut *self.usage.lock().expect("usage lock"))
    }

    /// One Analyst turn. `transcript` starts with the context message and
    /// alternates user/assistant, ending on a user message.
    pub async fn analyst_turn(
        &self,
        transcript: &[Message],
        must_draft: bool,
    ) -> Result<AnalystTurn, AgentError> {
        let mut messages: Vec<ChatMessage> = transcript.iter().map(ChatMessage::from).collect();
        if must_draft && let Some(last) = messages.last_mut() {
            last.content.push(serde_json::json!({"type": "text", "text":
                "[Round limit reached: produce the draft now, with anything unresolved under Open Questions.]"}));
        }
        self.converse(AgentRole::Analyst, ANALYST_PROMPT, messages, None)
            .await
    }

    /// Designs the `Architecture` section for a drafted spec.
    pub async fn design(
        &self,
        spec: &str,
        ws: Option<Workspace<'_>>,
    ) -> Result<DesignProposal, AgentError> {
        let msg = format!("<spec>\n{spec}\n</spec>{}", map_block(&ws));
        self.converse_tools(
            AgentRole::Architect,
            ARCHITECT_PROMPT,
            msg,
            ws,
            ToolKit::ReadOnly,
        )
        .await
    }

    pub async fn plan(
        &self,
        spec: &str,
        task: &str,
        ws: Option<Workspace<'_>>,
    ) -> Result<PlanProposal, AgentError> {
        let msg = format!(
            "<spec>\n{spec}\n</spec>\n\n<task>\n{task}\n</task>{}",
            map_block(&ws)
        );
        self.converse_tools(
            AgentRole::Planner,
            PLANNER_PROMPT,
            msg,
            ws,
            ToolKit::ReadOnly,
        )
        .await
    }

    pub async fn work(
        &self,
        spec: &str,
        task: &str,
        brief: &str,
        feedback: Option<&str>,
        ws: Option<Workspace<'_>>,
    ) -> Result<WorkProposal, AgentError> {
        let mut msg = format!("<spec>\n{spec}\n</spec>\n\n<task>\n{task}\n</task>");
        if !brief.trim().is_empty() {
            msg.push_str(&format!("\n\n<context_brief>\n{brief}\n</context_brief>"));
        }
        msg.push_str(&map_block(&ws));
        if let Some(fb) = feedback {
            msg.push_str(&format!(
                "\n\n<test_feedback>\nThe working copy still contains your previous attempt. The tests failed:\n{fb}\n</test_feedback>"
            ));
        }
        self.converse_tools(
            AgentRole::Worker,
            WORKER_PROMPT,
            msg,
            ws,
            ToolKit::ReadWrite,
        )
        .await
    }

    pub async fn test_feedback(
        &self,
        spec: &str,
        output: &str,
    ) -> Result<TestFeedback, AgentError> {
        let msg = format!("<spec>\n{spec}\n</spec>\n\n<test_output>\n{output}\n</test_output>");
        self.converse(
            AgentRole::Tester,
            TESTER_PROMPT,
            vec![ChatMessage::user_text(msg)],
            None,
        )
        .await
    }

    pub async fn document(
        &self,
        spec: &str,
        task: &str,
        work_summary: &str,
    ) -> Result<DocProposal, AgentError> {
        let msg = format!(
            "<spec>\n{spec}\n</spec>\n\n<task>\n{task}\n</task>\n\n<work_summary>\n{work_summary}\n</work_summary>"
        );
        self.converse(
            AgentRole::Documentor,
            DOCUMENTOR_PROMPT,
            vec![ChatMessage::user_text(msg)],
            None,
        )
        .await
    }

    async fn converse_tools<T: DeserializeOwned>(
        &self,
        role: AgentRole,
        prompt: &str,
        msg: String,
        ws: Option<Workspace<'_>>,
        kit: ToolKit,
    ) -> Result<T, AgentError> {
        let tools = ws.map(|w| (w.host, kit));
        self.converse(role, prompt, vec![ChatMessage::user_text(msg)], tools)
            .await
    }

    /// Calls the model until it replies without tool calls, running each tool
    /// call through the host. The final reply must be the role's JSON object.
    async fn converse<T: DeserializeOwned>(
        &self,
        role: AgentRole,
        prompt: &str,
        mut messages: Vec<ChatMessage>,
        mut tools: Option<(&mut dyn ToolHost, ToolKit)>,
    ) -> Result<T, AgentError> {
        let malformed = |reason: &str| AgentError::Malformed {
            agent: role.name(),
            reason: reason.to_string(),
        };
        let mut system = prompt.to_string();
        if let Some(skills) = self.skills.get(&role) {
            system.push_str(skills);
        }
        let defs = tools
            .as_ref()
            .map(|(_, kit)| kit.defs())
            .unwrap_or_default();
        let (mut calls_used, mut bytes_used, mut over_budget) = (0u32, 0usize, false);

        loop {
            let response = self
                .llm
                .chat(ChatRequest {
                    model: self.model(role).to_string(),
                    system: system.clone(),
                    messages: messages.clone(),
                    tools: defs.clone(),
                    max_tokens: MAX_TOKENS,
                    effort: self.models.effort.clone(),
                    cache: !defs.is_empty(),
                })
                .await?;
            self.record(role, response.usage, 0);
            let calls = response.tool_calls();
            if calls.is_empty() {
                return parse_json(role.name(), &response.text());
            }
            let Some((host, kit)) = tools.as_mut() else {
                return Err(malformed("called a tool, but it has none"));
            };
            if over_budget {
                return Err(malformed(
                    "kept calling tools after its tool budget ran out",
                ));
            }
            messages.push(response.into_message());

            let mut results = Vec::with_capacity(calls.len());
            for call in &calls {
                calls_used += 1;
                let (content, is_error) = if calls_used > self.limits.max_calls
                    || bytes_used > self.limits.max_bytes
                {
                    over_budget = true;
                    (
                            "Tool budget for this turn is used up. Do not call more tools: reply now with your final JSON object.".to_string(),
                            true,
                        )
                } else {
                    match ToolRequest::parse(call) {
                        Err(e) => (e, true),
                        Ok(req) if !kit.allows(&req) => (
                            format!("`{}` is not available to the {}", call.name, role.name()),
                            true,
                        ),
                        Ok(req) => match host.run(&req) {
                            Ok(text) => (text, false),
                            Err(e) => (e, true),
                        },
                    }
                };
                bytes_used += content.len();
                results.push(ToolResult {
                    tool_use_id: call.id.clone(),
                    content,
                    is_error,
                });
            }
            self.record(role, Usage::default(), calls.len() as u32);
            messages.push(ChatMessage::tool_results(results));
        }
    }

    fn model(&self, role: AgentRole) -> &str {
        let m = &self.models;
        match role {
            AgentRole::Analyst => &m.analyst,
            AgentRole::Architect => &m.architect,
            AgentRole::Planner => &m.planner,
            AgentRole::Worker => &m.worker,
            AgentRole::Tester => &m.tester,
            AgentRole::Documentor => &m.documentor,
        }
    }

    fn record(&self, role: AgentRole, tokens: Usage, tool_calls: u32) {
        let mut usage = self.usage.lock().expect("usage lock");
        let entry = usage.entry(role).or_default();
        if tool_calls == 0 {
            entry.calls += 1;
        }
        entry.tool_calls += tool_calls;
        entry.tokens += tokens;
    }
}

fn map_block(ws: &Option<Workspace<'_>>) -> String {
    match ws {
        Some(w) if !w.repo_map.is_empty() => {
            format!("\n\n<repo_map>\n{}\n</repo_map>", w.repo_map)
        }
        _ => String::new(),
    }
}

/// Extracts the outermost JSON object from a reply (tolerates code fences and
/// surrounding prose).
fn parse_json<T: DeserializeOwned>(agent: &'static str, reply: &str) -> Result<T, AgentError> {
    let malformed = |reason: String| AgentError::Malformed { agent, reason };
    let start = reply
        .find('{')
        .ok_or_else(|| malformed("no JSON object in reply".into()))?;
    let end = reply
        .rfind('}')
        .ok_or_else(|| malformed("no JSON object in reply".into()))?;
    if end < start {
        return Err(malformed("no JSON object in reply".into()));
    }
    serde_json::from_str(&reply[start..=end]).map_err(|e| malformed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_json() {
        let reply = "Here you go:\n```json\n{\"feedback\": \"fix {braces}\"}\n```";
        let fb: TestFeedback = parse_json("Tester", reply).unwrap();
        assert_eq!(fb.feedback, "fix {braces}");
    }

    #[test]
    fn analyst_turn_is_tagged() {
        let q: AnalystTurn = parse_json("Analyst", r#"{"type":"question","text":"Who?"}"#).unwrap();
        assert!(matches!(q, AnalystTurn::Question { .. }));
    }

    struct Host(Vec<ToolRequest>);
    impl ToolHost for Host {
        fn run(&mut self, request: &ToolRequest) -> Result<String, String> {
            self.0.push(request.clone());
            Ok("src/a.rs:1: fn a() {}".into())
        }
    }

    fn agents(llm: Arc<crate::llm::FakeLlm>, max_calls: u32) -> Agents {
        let m = |s: &str| s.to_string();
        Agents::new(
            llm,
            AgentModels {
                analyst: m("a"),
                architect: m("ar"),
                planner: m("p"),
                worker: m("w"),
                tester: m("t"),
                documentor: m("d"),
                effort: None,
            },
            HashMap::new(),
            ToolLimits {
                max_calls,
                max_bytes: 1_000,
            },
        )
    }

    #[tokio::test]
    async fn tool_loop_runs_calls_then_parses_final_reply() {
        use crate::llm::{FakeLlm, FakeReply};
        let llm = Arc::new(FakeLlm::new([
            FakeReply::tool("grep", serde_json::json!({"pattern": "fn a"})),
            FakeReply::tool("delete_file", serde_json::json!({"path": "x"})),
            r#"{"summary":"ok","context_brief":"see src/a.rs"}"#.into(),
        ]));
        let a = agents(llm.clone(), 10);
        let mut host = Host(Vec::new());
        let ws = Workspace {
            repo_map: "src/ (1 files)",
            host: &mut host,
        };
        let plan = a.plan("spec", "task", Some(ws)).await.unwrap();
        assert_eq!(plan.context_brief, "see src/a.rs");
        // The read-only Planner may not delete: the host never sees that call.
        assert_eq!(host.0.len(), 1);
        let reqs = llm.chat_requests();
        assert_eq!(reqs[0].tools.len(), 8);
        assert!(reqs[0].cache && reqs[0].messages[0].text().contains("<repo_map>"));
        let last = &reqs[2].messages;
        assert_eq!(last.len(), 5, "user, tool_use, result, tool_use, result");
        assert_eq!(last[4].content[0]["is_error"], true);
        let u = a.take_usage()[&AgentRole::Planner];
        assert_eq!((u.calls, u.tool_calls), (3, 2));
    }

    #[tokio::test]
    async fn budget_exhaustion_forces_an_answer_or_fails() {
        use crate::llm::{FakeLlm, FakeReply};
        let grep = || FakeReply::tool("grep", serde_json::json!({"pattern": "x"}));
        let llm = Arc::new(FakeLlm::new([grep(), grep(), grep()]));
        let a = agents(llm.clone(), 1);
        let mut host = Host(Vec::new());
        let ws = Workspace {
            repo_map: "",
            host: &mut host,
        };
        let err = a.plan("spec", "task", Some(ws)).await.err().unwrap();
        assert!(err.to_string().contains("tool budget"), "{err}");
        assert_eq!(host.0.len(), 1, "only the first call ran");
    }

    #[test]
    fn rejects_prose() {
        assert!(parse_json::<TestFeedback>("Tester", "all good").is_err());
    }
}
