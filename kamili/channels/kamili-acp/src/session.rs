//! One ACP session: a project, its engine, and at most one running job.
//!
//! A job is a spec session or a build. It runs as its own task and can span
//! several prompt turns: when the Analyst asks a question the turn ends, and
//! the user's next prompt is the answer. Each prompt waits for the job's next
//! [`TurnEnd`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use kamili::{
    BuildRequest, CancelToken, Engine, Error, GraphQuery, SpecOutcome, SpecRequest, check_skills,
    list_runs,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::channel::{AcpChannel, AnswerSlot, TurnEnd, message, session_update};
use crate::rpc::Out;

/// Slash commands, published to the client as available commands.
pub const COMMANDS: &[(&str, &str, Option<&str>)] = &[
    (
        "spec",
        "Write or revise a feature spec with the Analyst",
        Some("<feature> [--resume]"),
    ),
    (
        "build",
        "Build a task against an approved spec",
        Some("<feature> <task>"),
    ),
    (
        "resume",
        "Continue a cancelled or failed build",
        Some("[run id]"),
    ),
    ("runs", "List recent build runs", None),
    (
        "graph",
        "Show the most relevant files in the code graph",
        Some("[focus text]"),
    ),
    ("skills", "Show the skills each agent requires", None),
    ("help", "How to use Kamili here", None),
];

const HELP: &str = "Kamili builds features against specs.\n\n\
- `/spec <feature>`: agree a spec with the Analyst (answer its questions in the chat)\n\
- `/build <feature> <task>`: build a task against the approved spec\n\
- `/resume [run]`: continue a cancelled or failed build\n\
- `/runs`, `/graph [focus]`, `/skills`: look around\n\n\
You approve spec edits and the final write-back as they come up; nothing is written to your project before that.\n";

struct Job {
    turns: mpsc::UnboundedReceiver<TurnEnd>,
}

/// Interrupts a running job; shared outside the job lock so `session/cancel`
/// can reach it while a prompt is waiting.
#[derive(Default)]
struct Control {
    cancel: Option<CancelToken>,
    abort: Option<AbortHandle>,
}

pub struct Session {
    pub id: String,
    root: PathBuf,
    engine: Arc<Engine>,
    out: Arc<Out>,
    job: tokio::sync::Mutex<Option<Job>>,
    answer: AnswerSlot,
    cancelled: Arc<AtomicBool>,
    control: Mutex<Control>,
}

enum Command {
    Spec { feature: String, resume: bool },
    Build { feature: String, task: String },
    Resume { run: Option<String> },
    Runs,
    Graph { focus: Option<String> },
    Skills,
    Help,
}

fn parse_command(text: &str) -> Option<Command> {
    let text = text.trim();
    let rest = text.strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let args = args.trim();
    let first = || args.split_whitespace().next().map(str::to_string);
    Some(match name {
        "spec" => match first() {
            Some(feature) => Command::Spec {
                resume: args.split_whitespace().any(|a| a == "--resume"),
                feature,
            },
            None => Command::Help,
        },
        "build" => {
            let (feature, task) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
            let task = task.trim().trim_matches('"').trim();
            if feature.is_empty() || task.is_empty() {
                Command::Help
            } else {
                Command::Build {
                    feature: feature.to_string(),
                    task: task.to_string(),
                }
            }
        }
        "resume" => Command::Resume { run: first() },
        "runs" => Command::Runs,
        "graph" => Command::Graph {
            focus: (!args.is_empty()).then(|| args.to_string()),
        },
        "skills" => Command::Skills,
        _ => Command::Help,
    })
}

/// Plain text of a prompt: text blocks, and embedded text resources.
pub fn prompt_text(prompt: &Value) -> String {
    prompt
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| match b["type"].as_str() {
            Some("text") => b["text"].as_str().map(str::to_string),
            Some("resource") => b["resource"]["text"].as_str().map(str::to_string),
            Some("resource_link") => b["uri"].as_str().map(|u| format!("<{u}>")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Session {
    pub fn new(id: String, root: PathBuf, engine: Engine, out: Arc<Out>) -> Self {
        Self {
            id,
            root,
            engine: Arc::new(engine),
            out,
            job: tokio::sync::Mutex::new(None),
            answer: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(AtomicBool::new(false)),
            control: Mutex::new(Control::default()),
        }
    }

    pub fn announce_commands(&self) {
        let commands: Vec<Value> = COMMANDS
            .iter()
            .map(|(name, description, hint)| {
                let mut c = json!({"name": name, "description": description});
                if let Some(h) = hint {
                    c["input"] = json!({"hint": h});
                }
                c
            })
            .collect();
        session_update(
            &self.out,
            &self.id,
            json!({"sessionUpdate": "available_commands_update", "availableCommands": commands}),
        );
    }

    fn say(&self, text: &str) {
        message(&self.out, &self.id, text);
    }

    /// One prompt turn. Returns the ACP stop reason.
    pub async fn prompt(&self, text: String) -> &'static str {
        self.cancelled.store(false, Ordering::SeqCst);
        let mut job = self.job.lock().await;

        if let Some(running) = job.as_mut() {
            let pending = self.answer.lock().expect("answer slot").take();
            match (pending, parse_command(&text)) {
                // A plain prompt answers the open question.
                (Some(tx), None) => {
                    let _ = tx.send(Some(text));
                    return self.wait(&mut job).await;
                }
                // A new command abandons the open question first.
                (Some(tx), Some(_)) => {
                    let _ = tx.send(None);
                    while let Some(end) = running.turns.recv().await {
                        if let TurnEnd::Finished(report) = end {
                            self.say(&report);
                            break;
                        }
                    }
                    *job = None;
                }
                (None, _) => {
                    // A job without an open question shouldn't outlive its
                    // turn; wait for it rather than run two at once.
                    return self.wait(&mut job).await;
                }
            }
        }

        let Some(command) = parse_command(&text) else {
            self.say(HELP);
            return "end_turn";
        };
        match command {
            Command::Help => self.say(HELP),
            Command::Runs => self.say(&runs_report(&self.root)),
            Command::Skills => self.say(&skills_report(&self.engine, &self.root)),
            Command::Graph { focus } => {
                let query = GraphQuery::Overview { focus, limit: 15 };
                match kamili::explore_graph(self.engine.config(), &self.root, &query) {
                    Ok(text) => self.say(&format!("```\n{text}```\n")),
                    Err(e) => self.say(&format!("Error: {e}\n")),
                }
            }
            Command::Spec { feature, resume } => {
                let req = SpecRequest {
                    project_root: self.root.clone(),
                    feature: feature.clone(),
                    resume,
                };
                *job = Some(self.start(None, move |engine, ch| async move {
                    match engine.spec_session(req, &ch).await {
                        Ok(SpecOutcome::Approved { path }) => {
                            format!("✅ Spec approved and written: `{}`\n", path.display())
                        }
                        Ok(SpecOutcome::Rejected) => "Draft rejected; nothing written.\n".into(),
                        Ok(SpecOutcome::Abandoned) => {
                            format!("Paused. Continue with `/spec {feature} --resume`.\n")
                        }
                        Ok(_) => "Spec session ended.\n".into(),
                        Err(e) => error_report(&e),
                    }
                }));
                return self.wait(&mut job).await;
            }
            Command::Build { feature, task } => {
                let req = BuildRequest {
                    project_root: self.root.clone(),
                    feature,
                    task,
                    ..Default::default()
                };
                *job = Some(self.build(req));
                return self.wait(&mut job).await;
            }
            Command::Resume { run } => {
                let req = BuildRequest {
                    project_root: self.root.clone(),
                    resume: Some(run.unwrap_or_else(|| "last".into())),
                    ..Default::default()
                };
                *job = Some(self.build(req));
                return self.wait(&mut job).await;
            }
        }
        "end_turn"
    }

    fn build(&self, mut req: BuildRequest) -> Job {
        let token = CancelToken::new();
        req.cancel = Some(token.clone());
        self.start(Some(token), move |engine, ch| async move {
            match engine.build(req, &ch).await {
                Ok(out) => {
                    let mut s = format!(
                        "✅ Done: {} file(s) changed{} in {} attempt(s) (run `{}`).\n",
                        out.files_changed.len(),
                        if out.spec_changed {
                            ", spec updated"
                        } else {
                            ""
                        },
                        out.attempts,
                        out.run_id
                    );
                    for f in &out.files_changed {
                        s.push_str(&format!("- `{f}`\n"));
                    }
                    s
                }
                Err(e) => error_report(&e),
            }
        })
    }

    /// Spawns a job. `run` gets the engine and this session's channel and
    /// returns the final report.
    fn start<F, Fut>(&self, cancel: Option<CancelToken>, run: F) -> Job
    where
        F: FnOnce(Arc<Engine>, AcpChannel) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = String> + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        let ch = AcpChannel::new(
            self.id.clone(),
            self.out.clone(),
            tx.clone(),
            self.answer.clone(),
            self.cancelled.clone(),
        );
        let engine = self.engine.clone();
        let cancelled = self.cancelled.clone();
        let handle = tokio::spawn(async move {
            let report = run(engine, ch).await;
            let end = if cancelled.load(Ordering::SeqCst) {
                TurnEnd::Cancelled
            } else {
                TurnEnd::Finished(report)
            };
            let _ = tx.send(end);
        });
        *self.control.lock().expect("control") = Control {
            cancel,
            abort: Some(handle.abort_handle()),
        };
        Job { turns: rx }
    }

    /// Waits until the job asks something or finishes.
    async fn wait(&self, job: &mut Option<Job>) -> &'static str {
        let Some(running) = job.as_mut() else {
            return "end_turn";
        };
        match running.turns.recv().await {
            Some(TurnEnd::Asked) => "end_turn",
            Some(TurnEnd::Finished(report)) => {
                self.say(&report);
                *job = None;
                "end_turn"
            }
            // Cancelled, or the task was aborted (channel closed).
            Some(TurnEnd::Cancelled) | None => {
                *job = None;
                "cancelled"
            }
        }
    }

    /// `session/cancel`: stop the running job at its next safe point.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        // An open question is answered with "no answer" (the session pauses).
        if let Some(tx) = self.answer.lock().expect("answer slot").take() {
            let _ = tx.send(None);
        }
        let control = self.control.lock().expect("control");
        match (&control.cancel, &control.abort) {
            // Builds stop cleanly and stay resumable.
            (Some(token), _) => token.cancel(),
            // A spec session saves its transcript after every answer, so
            // aborting a model call loses nothing that was answered.
            (None, Some(abort)) => abort.abort(),
            _ => {}
        }
    }
}

fn error_report(e: &Error) -> String {
    match e {
        Error::Cancelled(run) => format!("Cancelled. Continue with `/resume {run}`.\n"),
        other => format!("❌ {other} (exit code {})\n", other.exit_code()),
    }
}

fn runs_report(root: &std::path::Path) -> String {
    let runs = list_runs(root);
    if runs.is_empty() {
        return "No build runs yet.\n".into();
    }
    let mut s = String::from("| Run | State | Task |\n|---|---|---|\n");
    for r in runs.iter().take(15) {
        s.push_str(&format!(
            "| `{}` | {}{} | {} |\n",
            r.id,
            r.state.replace('|', "/"),
            if r.resumable { " ↻" } else { "" },
            r.task.replace('|', "/")
        ));
    }
    s.push_str("\n↻ = resumable with `/resume <run>`\n");
    s
}

fn skills_report(engine: &Engine, root: &std::path::Path) -> String {
    match check_skills(engine.config(), root) {
        Ok(list) if list.is_empty() => {
            "No required skills (add them under `[skills.required]` in `.kamili.toml`).\n".into()
        }
        Ok(list) => {
            let mut s = String::new();
            for st in list {
                let mark = if st.found.is_some() {
                    "✅"
                } else {
                    "❌ missing"
                };
                s.push_str(&format!("- {mark} `{}` ({})\n", st.reference, st.agent));
            }
            s
        }
        Err(e) => format!("Error: {e}\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert!(matches!(
            parse_command("/build greet add a \"hello\" file"),
            Some(Command::Build { feature, task }) if feature == "greet" && task == "add a \"hello\" file"
        ));
        assert!(matches!(
            parse_command(" /spec greet --resume "),
            Some(Command::Spec { feature, resume: true }) if feature == "greet"
        ));
        assert!(matches!(parse_command("/build greet"), Some(Command::Help)));
        assert!(matches!(
            parse_command("/resume"),
            Some(Command::Resume { run: None })
        ));
        assert!(
            matches!(parse_command("/graph login flow"), Some(Command::Graph { focus: Some(f) }) if f == "login flow")
        );
        assert!(parse_command("Alice, mostly").is_none());
    }

    #[test]
    fn prompt_text_joins_blocks() {
        let p = json!([
            {"type": "text", "text": "fix"},
            {"type": "resource", "resource": {"uri": "file:///a", "text": "body"}},
            {"type": "image", "data": "...", "mimeType": "image/png"}
        ]);
        assert_eq!(prompt_text(&p), "fix\nbody");
    }
}
