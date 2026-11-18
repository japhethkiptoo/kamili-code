//! Engine ⇄ UI bridge. No GPUI here, so it is tested on its own.
//!
//! The engine runs on tokio; the UI runs on GPUI's executor. They talk through
//! one unbounded channel ([`ToUi`]) plus a oneshot per question or approval:
//!
//! - `event` must return at once, so it only pushes onto the channel.
//! - `ask` and `approve` push a request carrying a oneshot sender, then wait
//!   for the user. If the window goes away, the sender is dropped and the
//!   engine sees "no answer" or [`Decision::Reject`], the safe defaults.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use kamili::{
    ApprovalRequest, BuildRequest, CancelToken, Channel, Decision, Engine, Error, Event, Question,
    SpecOutcome, SpecRequest,
};
use tokio::sync::{mpsc, oneshot};

/// Builds an engine for a project. The composition root (the CLI) supplies
/// this, since it owns configuration loading and credentials.
#[async_trait]
pub trait EngineFactory: Send + Sync {
    async fn engine(&self, project_root: &Path) -> Result<Engine, Error>;
}

/// Everything the engine side sends to the UI.
#[derive(Debug)]
pub enum ToUi {
    Event(Event),
    Question {
        text: String,
        reply: oneshot::Sender<Option<String>>,
    },
    Approval {
        request: ApprovalRequest,
        reply: oneshot::Sender<Decision>,
    },
    /// A job ended, successfully or not.
    Finished(Finished),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub ok: bool,
    pub message: String,
    /// Kamili's exit code for this outcome (0 on success).
    pub exit_code: u8,
}

pub type UiSender = mpsc::UnboundedSender<ToUi>;
pub type UiReceiver = mpsc::UnboundedReceiver<ToUi>;

pub fn ui_channel() -> (UiSender, UiReceiver) {
    mpsc::unbounded_channel()
}

/// The [`Channel`] the engine talks to.
pub struct DesktopChannel {
    tx: UiSender,
}

impl DesktopChannel {
    pub fn new(tx: UiSender) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl Channel for DesktopChannel {
    fn event(&self, event: Event) {
        // A closed window just means nobody is watching.
        let _ = self.tx.send(ToUi::Event(event));
    }

    async fn ask(&self, question: &Question) -> Option<String> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(ToUi::Question {
                text: question.text.clone(),
                reply,
            })
            .ok()?;
        answer.await.ok().flatten()
    }

    async fn approve(&self, request: &ApprovalRequest) -> Decision {
        let (reply, decision) = oneshot::channel();
        if self
            .tx
            .send(ToUi::Approval {
                request: request.clone(),
                reply,
            })
            .is_err()
        {
            return Decision::Reject;
        }
        decision.await.unwrap_or(Decision::Reject)
    }
}

/// What to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    Build {
        feature: String,
        task: String,
        force: bool,
        docs_only: bool,
    },
    /// `run` is a run id, or `last`.
    Resume {
        run: String,
    },
    Spec {
        feature: String,
        resume: bool,
    },
}

/// Starts a job on the tokio runtime. Progress and the result arrive on `tx`.
/// Builds stop at their next safe point when the returned token is cancelled
/// (they stay resumable); spec sessions are paused by answering "no answer".
pub fn start(
    runtime: &tokio::runtime::Handle,
    factory: Arc<dyn EngineFactory>,
    project_root: PathBuf,
    job: Job,
    tx: UiSender,
) -> CancelToken {
    let cancel = CancelToken::new();
    let token = cancel.clone();
    runtime.spawn(async move {
        let finished = run_job(factory.as_ref(), project_root, job, token, &tx).await;
        let _ = tx.send(ToUi::Finished(finished));
    });
    cancel
}

async fn run_job(
    factory: &dyn EngineFactory,
    project_root: PathBuf,
    job: Job,
    cancel: CancelToken,
    tx: &UiSender,
) -> Finished {
    let engine = match factory.engine(&project_root).await {
        Ok(e) => e,
        Err(e) => return failed(&e),
    };
    let channel = DesktopChannel::new(tx.clone());
    match job {
        Job::Spec { feature, resume } => {
            let request = SpecRequest {
                project_root,
                feature: feature.clone(),
                resume,
            };
            match engine.spec_session(request, &channel).await {
                Ok(outcome) => spec_finished(&feature, &outcome),
                Err(e) => failed(&e),
            }
        }
        Job::Build { .. } | Job::Resume { .. } => {
            let request = build_request(project_root, job, cancel);
            match engine.build(request, &channel).await {
                Ok(o) => Finished {
                    ok: true,
                    message: build_summary(&o.run_id, o.attempts, &o.files_changed, o.spec_changed),
                    exit_code: 0,
                },
                Err(e) => failed(&e),
            }
        }
    }
}

fn build_request(project_root: PathBuf, job: Job, cancel: CancelToken) -> BuildRequest {
    let (feature, task, force, docs_only, resume) = match job {
        Job::Build {
            feature,
            task,
            force,
            docs_only,
        } => (feature, task, force, docs_only, None),
        Job::Resume { run } => (String::new(), String::new(), false, false, Some(run)),
        Job::Spec { .. } => unreachable!("spec jobs are not builds"),
    };
    BuildRequest {
        project_root,
        feature,
        task,
        force,
        docs_only,
        resume,
        cancel: Some(cancel),
    }
}

fn failed(e: &Error) -> Finished {
    Finished {
        ok: false,
        message: e.to_string(),
        exit_code: e.exit_code(),
    }
}

pub fn build_summary(run_id: &str, attempts: u32, files: &[String], spec_changed: bool) -> String {
    let mut s = format!("Run {run_id} finished");
    if attempts > 0 {
        s.push_str(&format!(
            " after {attempts} attempt{}",
            if attempts == 1 { "" } else { "s" }
        ));
    }
    s.push_str(&format!(
        ": {} file{} changed",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    ));
    if spec_changed {
        s.push_str(", spec updated");
    }
    s
}

fn spec_finished(feature: &str, outcome: &SpecOutcome) -> Finished {
    let message = match outcome {
        SpecOutcome::Approved { path } => {
            format!("Spec approved and written to {}", path.display())
        }
        SpecOutcome::Rejected => "Draft rejected; nothing written".to_string(),
        SpecOutcome::Abandoned => format!("Paused; resume the `{feature}` spec to continue"),
        _ => "Spec session ended".to_string(),
    };
    Finished {
        ok: matches!(outcome, SpecOutcome::Approved { .. }),
        message,
        exit_code: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kamili::{ApprovalKind, Stage};

    fn approval() -> ApprovalRequest {
        ApprovalRequest {
            kind: ApprovalKind::WriteBack,
            summary: "write 1 file".into(),
            diff: None,
            changes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn events_are_forwarded_without_blocking() {
        let (tx, mut rx) = ui_channel();
        let ch = DesktopChannel::new(tx);
        ch.event(Event::StageStarted { stage: Stage::Plan });
        assert!(matches!(
            rx.recv().await,
            Some(ToUi::Event(Event::StageStarted { stage: Stage::Plan }))
        ));
    }

    #[tokio::test]
    async fn question_waits_for_the_ui_answer() {
        let (tx, mut rx) = ui_channel();
        let ch = DesktopChannel::new(tx);
        let ui = tokio::spawn(async move {
            match rx.recv().await {
                Some(ToUi::Question { text, reply }) => {
                    assert_eq!(text, "Who uses it?");
                    reply.send(Some("admins".into())).unwrap();
                }
                other => panic!("unexpected {other:?}"),
            }
        });
        let q = Question {
            text: "Who uses it?".into(),
        };
        assert_eq!(ch.ask(&q).await.as_deref(), Some("admins"));
        ui.await.unwrap();
    }

    #[tokio::test]
    async fn approval_follows_the_ui_decision() {
        let (tx, mut rx) = ui_channel();
        let ch = DesktopChannel::new(tx);
        tokio::spawn(async move {
            if let Some(ToUi::Approval { reply, .. }) = rx.recv().await {
                reply.send(Decision::Approve).unwrap();
            }
        });
        assert_eq!(ch.approve(&approval()).await, Decision::Approve);
    }

    #[tokio::test]
    async fn closed_window_means_reject_and_no_answer() {
        let (tx, rx) = ui_channel();
        drop(rx);
        let ch = DesktopChannel::new(tx);
        assert_eq!(ch.approve(&approval()).await, Decision::Reject);
        let q = Question { text: "?".into() };
        assert_eq!(ch.ask(&q).await, None);
    }

    #[tokio::test]
    async fn dropped_reply_means_reject() {
        let (tx, mut rx) = ui_channel();
        let ch = DesktopChannel::new(tx);
        tokio::spawn(async move {
            // The UI received the request but dropped it (e.g. view closed).
            let _ = rx.recv().await;
        });
        assert_eq!(ch.approve(&approval()).await, Decision::Reject);
    }

    #[test]
    fn summaries_read_well() {
        assert_eq!(
            build_summary("r1", 1, &["a.rs".into()], false),
            "Run r1 finished after 1 attempt: 1 file changed"
        );
        assert_eq!(
            build_summary("r2", 2, &["a".into(), "b".into()], true),
            "Run r2 finished after 2 attempts: 2 files changed, spec updated"
        );
        assert_eq!(
            build_summary("r3", 0, &[], true),
            "Run r3 finished: 0 files changed, spec updated"
        );
    }

    #[test]
    fn resume_builds_a_resume_request() {
        let req = build_request(
            PathBuf::from("/p"),
            Job::Resume { run: "last".into() },
            CancelToken::new(),
        );
        assert_eq!(req.resume.as_deref(), Some("last"));
        assert!(req.cancel.is_some());
    }
}
