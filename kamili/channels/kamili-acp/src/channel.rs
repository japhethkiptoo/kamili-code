//! Kamili's `Channel` contract over ACP.
//!
//! | Engine | ACP |
//! |---|---|
//! | `AgentMessage`, `SpecDiff`, warnings | `agent_message_chunk` |
//! | info logs, spec-session stages | `agent_thought_chunk` |
//! | `StateChanged` | `plan` (Plan → Work → Test → Document → Write back) |
//! | `ToolUse`, `TestResult` | `tool_call` (already finished) |
//! | `ask` | the question as a message; the turn ends; the next prompt answers |
//! | `approve` | `tool_call` with real file diffs, then `session/request_permission` |

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kamili::{
    ApprovalKind, ApprovalRequest, Channel, Decision, Event, LogLevel, Question, RunStep,
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::rpc::Out;

/// Why a prompt turn ended, sent by a running job to the waiting prompt.
#[derive(Debug)]
pub enum TurnEnd {
    /// The engine asked a question; the next prompt is the answer.
    Asked,
    /// The job finished; the text is its final report.
    Finished(String),
    /// The job was cancelled.
    Cancelled,
}

/// A question waiting for the user's next prompt.
pub type AnswerSlot = Arc<Mutex<Option<oneshot::Sender<Option<String>>>>>;

const APPROVE: &str = "approve";
const REJECT: &str = "reject";

const PLAN: [(RunStep, &str); 5] = [
    (
        RunStep::Plan,
        "Plan: reconcile the task with the spec (Planner)",
    ),
    (RunStep::Work, "Implement in a working copy (Worker)"),
    (RunStep::Test, "Run the tests"),
    (
        RunStep::Document,
        "Update the spec's docs sections (Documentor)",
    ),
    (
        RunStep::WriteBack,
        "Write back to the project (needs your approval)",
    ),
];

pub struct AcpChannel {
    pub session_id: String,
    pub out: Arc<Out>,
    pub turns: mpsc::UnboundedSender<TurnEnd>,
    pub answer: AnswerSlot,
    /// Set by `session/cancel`: approvals are rejected from then on.
    pub cancelled: Arc<AtomicBool>,
    next_tool: AtomicU64,
}

impl AcpChannel {
    pub fn new(
        session_id: String,
        out: Arc<Out>,
        turns: mpsc::UnboundedSender<TurnEnd>,
        answer: AnswerSlot,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            session_id,
            out,
            turns,
            answer,
            cancelled,
            next_tool: AtomicU64::new(1),
        }
    }

    fn update(&self, update: Value) {
        session_update(&self.out, &self.session_id, update);
    }

    fn tool_id(&self) -> String {
        format!("kamili_{}", self.next_tool.fetch_add(1, Ordering::Relaxed))
    }

    fn plan(&self, step: RunStep) {
        let current = PLAN.iter().position(|(s, _)| *s == step);
        let entries: Vec<Value> = PLAN
            .iter()
            .enumerate()
            .map(|(i, (_, label))| {
                let status = match (step, current) {
                    (RunStep::Done, _) => "completed",
                    (_, Some(c)) if i < c => "completed",
                    (_, Some(c)) if i == c => "in_progress",
                    _ => "pending",
                };
                json!({"content": label, "priority": "medium", "status": status})
            })
            .collect();
        self.update(json!({"sessionUpdate": "plan", "entries": entries}));
    }

    /// A tool call that is already over, e.g. a file read or a test run.
    fn finished_tool(&self, title: String, kind: &str, ok: bool, detail: Option<String>) {
        let mut call = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": self.tool_id(),
            "title": title,
            "kind": kind,
            "status": if ok { "completed" } else { "failed" },
        });
        if let Some(text) = detail {
            call["content"] = json!([{"type": "content", "content": text_block(&text)}]);
        }
        self.update(call);
    }
}

pub fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

pub fn session_update(out: &Out, session_id: &str, update: Value) {
    out.notify(
        "session/update",
        json!({"sessionId": session_id, "update": update}),
    );
}

pub fn message(out: &Out, session_id: &str, text: &str) {
    session_update(
        out,
        session_id,
        json!({"sessionUpdate": "agent_message_chunk", "content": text_block(text)}),
    );
}

fn thought(out: &Out, session_id: &str, text: &str) {
    session_update(
        out,
        session_id,
        json!({"sessionUpdate": "agent_thought_chunk", "content": text_block(text)}),
    );
}

/// ACP tool kind for a Kamili tool description (`grep "x" in src`, …).
fn tool_kind(description: &str) -> &'static str {
    match description.split_whitespace().next().unwrap_or_default() {
        "read" | "list" | "outline" => "read",
        "grep" | "find" | "references" | "related" => "search",
        "edit" | "write" => "edit",
        "delete" => "delete",
        _ => "other",
    }
}

#[async_trait]
impl Channel for AcpChannel {
    fn event(&self, event: Event) {
        let sid = &self.session_id;
        match event {
            Event::AgentMessage { agent, text } => {
                message(&self.out, sid, &format!("**{agent}:** {text}\n\n"));
            }
            Event::SpecDiff { diff, .. } => {
                message(&self.out, sid, &format!("```diff\n{diff}```\n\n"));
            }
            Event::StateChanged { step, state, .. } => {
                if step != RunStep::Stopped {
                    self.plan(step);
                }
                thought(&self.out, sid, &format!("{state}\n"));
            }
            Event::ToolUse { agent, description } => {
                let kind = tool_kind(&description);
                self.finished_tool(format!("{agent}: {description}"), kind, true, None);
            }
            Event::TestResult {
                attempt,
                passed,
                summary,
            } => {
                self.finished_tool(
                    format!("Tests (attempt {attempt})"),
                    "execute",
                    passed,
                    Some(summary),
                );
            }
            Event::Log {
                level: LogLevel::Warn,
                message: m,
            } => message(&self.out, sid, &format!("⚠️ {m}\n\n")),
            Event::Log { message: m, .. } => thought(&self.out, sid, &format!("{m}\n")),
            Event::StageStarted { stage } => thought(&self.out, sid, &format!("{stage}…\n")),
            _ => {}
        }
    }

    async fn ask(&self, question: &Question) -> Option<String> {
        if self.cancelled.load(Ordering::SeqCst) {
            return None;
        }
        message(&self.out, &self.session_id, &format!("{}\n", question.text));
        let (tx, rx) = oneshot::channel();
        *self.answer.lock().expect("answer slot") = Some(tx);
        let _ = self.turns.send(TurnEnd::Asked);
        rx.await.ok().flatten()
    }

    async fn approve(&self, request: &ApprovalRequest) -> Decision {
        if self.cancelled.load(Ordering::SeqCst) {
            return Decision::Reject;
        }
        let id = self.tool_id();
        let title = match request.kind {
            ApprovalKind::ApplySpecEdits => "Approve the Planner's spec edits",
            ApprovalKind::WriteSpec => "Approve and write the spec",
            ApprovalKind::WriteBack => "Approve writing the changes to the project",
            _ => "Approve",
        };
        // Real diffs where we have file contents; the summary as text.
        let mut content: Vec<Value> = request
            .changes
            .iter()
            .map(|c| {
                json!({
                    "type": "diff",
                    "path": c.path.to_string_lossy(),
                    "oldText": c.old_text,
                    "newText": c.new_text.clone().unwrap_or_default(),
                })
            })
            .collect();
        content.push(json!({"type": "content", "content": text_block(&request.summary)}));
        let locations: Vec<Value> = request
            .changes
            .iter()
            .map(|c| json!({"path": c.path.to_string_lossy()}))
            .collect();
        self.update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": id,
            "title": title,
            "kind": "edit",
            "status": "pending",
            "content": content,
            "locations": locations,
        }));

        let response = self
            .out
            .request(
                "session/request_permission",
                json!({
                    "sessionId": self.session_id,
                    "toolCall": {"toolCallId": id, "title": title, "kind": "edit", "status": "pending"},
                    "options": [
                        {"optionId": APPROVE, "name": "Approve", "kind": "allow_once"},
                        {"optionId": REJECT, "name": "Reject", "kind": "reject_once"},
                    ],
                }),
            )
            .await;
        let approved = !self.cancelled.load(Ordering::SeqCst)
            && response.is_ok_and(|r| {
                r["outcome"]["outcome"] == "selected" && r["outcome"]["optionId"] == APPROVE
            });
        self.update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": id,
            "status": if approved { "completed" } else { "failed" },
        }));
        if approved {
            Decision::Approve
        } else {
            Decision::Reject
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_kinds() {
        assert_eq!(tool_kind("grep \"x\" in src"), "search");
        assert_eq!(tool_kind("read src/a.rs:1-9"), "read");
        assert_eq!(tool_kind("edit src/a.rs"), "edit");
        assert_eq!(tool_kind("delete x"), "delete");
        assert_eq!(tool_kind("related to a.rs"), "search");
    }
}
