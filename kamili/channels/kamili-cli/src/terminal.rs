//! Interactive terminal channel: events to stderr, questions and approvals on stdin.

use async_trait::async_trait;
use kamili::{ApprovalRequest, Channel, Decision, Event, LogLevel, Question};

pub struct TerminalChannel {
    pub auto_approve: bool,
}

#[async_trait]
impl Channel for TerminalChannel {
    fn event(&self, event: Event) {
        print_event(&event);
    }

    async fn ask(&self, question: &Question) -> Option<String> {
        eprintln!("\n? {}\n  (empty line or /quit to pause)", question.text);
        read_line("> ")
            .await
            .filter(|a| !a.is_empty() && a != "/quit")
    }

    async fn approve(&self, request: &ApprovalRequest) -> Decision {
        eprintln!("\n== approval: {:?} ==\n{}", request.kind, request.summary);
        if self.auto_approve {
            eprintln!("(approved by --yes)");
            return Decision::Approve;
        }
        match read_line("Approve? [y/N] ").await.as_deref() {
            Some("y" | "Y" | "yes") => Decision::Approve,
            _ => Decision::Reject,
        }
    }
}

pub fn print_event(event: &Event) {
    match event {
        Event::StageStarted { stage } => eprintln!("→ {stage}"),
        Event::StateChanged { state, .. } => eprintln!("→ {state}"),
        Event::ToolUse { agent, description } => eprintln!("    · {agent}: {description}"),
        Event::StageFinished { .. } => {}
        Event::AgentMessage { agent, text } => eprintln!("  [{agent}] {text}"),
        Event::SpecDiff { diff, .. } => eprintln!("{diff}"),
        Event::TestResult {
            attempt,
            passed,
            summary,
        } => {
            eprintln!(
                "  tests (attempt {attempt}): {} {summary}",
                if *passed { "PASS" } else { "FAIL" }
            )
        }
        Event::Log {
            level: LogLevel::Warn,
            message,
        } => eprintln!("  warning: {message}"),
        Event::Log { message, .. } => eprintln!("  {message}"),
        other => eprintln!("  {other:?}"),
    }
}

/// Reads one trimmed line from stdin; `None` on EOF or error.
async fn read_line(prompt: &str) -> Option<String> {
    eprint!("{prompt}");
    tokio::task::spawn_blocking(|| {
        let mut buf = String::new();
        match std::io::stdin().read_line(&mut buf) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(buf.trim().to_string()),
        }
    })
    .await
    .ok()
    .flatten()
}
