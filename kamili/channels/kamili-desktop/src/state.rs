//! What the window shows, as plain data. The UI renders it; [`AppState::apply`]
//! updates it from engine messages. No GPUI here, so it is tested on its own.

use std::path::{Path, PathBuf};

use kamili::{
    AgentName, ApprovalKind, ApprovalRequest, Decision, Event, FileChangePreview, LogLevel,
    RunStep, Stage,
};
use similar::{ChangeTag, TextDiff};
use tokio::sync::oneshot;

use crate::bridge::{Finished, ToUi};

/// The five build steps, in order, as shown in the progress bar.
pub const BUILD_STEPS: [(RunStep, &str); 5] = [
    (RunStep::Plan, "Plan"),
    (RunStep::Work, "Work"),
    (RunStep::Test, "Test"),
    (RunStep::Document, "Document"),
    (RunStep::WriteBack, "Write back"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Active,
    Done,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Build,
    Spec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedKind {
    /// Something an agent said.
    Agent(AgentName),
    /// A tool an agent called.
    Tool(AgentName),
    /// An Analyst question.
    Question,
    /// The user's answer.
    Answer,
    Stage,
    Info,
    Warn,
    Test {
        passed: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedItem {
    pub kind: FeedKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestAttempt {
    pub attempt: u32,
    pub passed: bool,
    pub summary: String,
}

/// The job in progress (or the last one).
#[derive(Debug)]
pub struct JobView {
    pub kind: JobKind,
    pub title: String,
    pub run_id: Option<String>,
    /// Human-readable run state, e.g. `working (attempt 2)`.
    pub state: String,
    pub steps: [StepState; 5],
    pub feed: Vec<FeedItem>,
    pub tests: Vec<TestAttempt>,
    pub finished: Option<Finished>,
}

impl JobView {
    pub fn new(kind: JobKind, title: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            run_id: None,
            state: "starting".into(),
            steps: [StepState::Pending; 5],
            feed: Vec::new(),
            tests: Vec::new(),
            finished: None,
        }
    }

    pub fn running(&self) -> bool {
        self.finished.is_none()
    }

    fn push(&mut self, kind: FeedKind, text: impl Into<String>) {
        self.feed.push(FeedItem {
            kind,
            text: text.into(),
        });
    }

    fn set_step(&mut self, step: RunStep) {
        let index = BUILD_STEPS.iter().position(|(s, _)| *s == step);
        match (step, index) {
            (_, Some(i)) => {
                for (j, s) in self.steps.iter_mut().enumerate() {
                    *s = match j.cmp(&i) {
                        std::cmp::Ordering::Less => StepState::Done,
                        std::cmp::Ordering::Equal => StepState::Active,
                        std::cmp::Ordering::Greater => StepState::Pending,
                    };
                }
            }
            (RunStep::Done, _) => self.steps = [StepState::Done; 5],
            (RunStep::Stopped, _) => {
                for s in &mut self.steps {
                    if *s == StepState::Active {
                        *s = StepState::Failed;
                    }
                }
            }
            _ => {}
        }
    }
}

/// An approval waiting for the user, with its diffs ready to render.
#[derive(Debug)]
pub struct PendingApproval {
    pub kind: ApprovalKind,
    pub summary: String,
    pub files: Vec<FileDiff>,
    /// The spec diff, when the approval has one but no file previews.
    pub spec_diff: Option<Vec<DiffLine>>,
    reply: Option<oneshot::Sender<Decision>>,
}

impl PendingApproval {
    pub fn title(&self) -> &'static str {
        match self.kind {
            ApprovalKind::WriteBack => "Write changes to your project?",
            ApprovalKind::WriteSpec => "Write and approve this spec?",
            ApprovalKind::ApplySpecEdits => "Accept the Planner's spec edits?",
            _ => "Approve?",
        }
    }
}

#[derive(Debug)]
pub struct PendingQuestion {
    pub text: String,
    reply: Option<oneshot::Sender<Option<String>>>,
}

#[derive(Debug, Default)]
pub struct AppState {
    pub job: Option<JobView>,
    pub approval: Option<PendingApproval>,
    pub question: Option<PendingQuestion>,
}

impl AppState {
    pub fn start_job(&mut self, kind: JobKind, title: impl Into<String>) {
        self.job = Some(JobView::new(kind, title));
        self.approval = None;
        self.question = None;
    }

    pub fn busy(&self) -> bool {
        self.job.as_ref().is_some_and(JobView::running)
    }

    pub fn apply(&mut self, msg: ToUi, project_root: &Path) {
        match msg {
            ToUi::Event(e) => self.apply_event(e),
            ToUi::Question { text, reply } => {
                if let Some(job) = &mut self.job {
                    job.push(FeedKind::Question, text.clone());
                }
                self.question = Some(PendingQuestion {
                    text,
                    reply: Some(reply),
                });
            }
            ToUi::Approval { request, reply } => {
                self.approval = Some(pending_approval(request, reply, project_root));
            }
            ToUi::Finished(f) => {
                // Unanswered prompts are moot now; dropping them rejects.
                self.approval = None;
                self.question = None;
                if let Some(job) = &mut self.job {
                    if !f.ok {
                        job.set_step(RunStep::Stopped);
                    }
                    job.push(
                        if f.ok { FeedKind::Info } else { FeedKind::Warn },
                        f.message.clone(),
                    );
                    job.finished = Some(f);
                }
            }
        }
    }

    fn apply_event(&mut self, event: Event) {
        let Some(job) = &mut self.job else { return };
        match event {
            Event::StageStarted { stage } => {
                job.push(FeedKind::Stage, stage_title(stage));
                if let Some(step) = stage_step(stage) {
                    job.set_step(step);
                }
            }
            Event::StageFinished { .. } => {}
            Event::AgentMessage { agent, text } => job.push(FeedKind::Agent(agent), text),
            Event::ToolUse { agent, description } => job.push(FeedKind::Tool(agent), description),
            Event::SpecDiff { feature, .. } => {
                job.push(FeedKind::Info, format!("Spec diff ready for `{feature}`"))
            }
            Event::TestResult {
                attempt,
                passed,
                summary,
            } => {
                job.push(
                    FeedKind::Test { passed },
                    format!(
                        "Attempt {attempt}: tests {}",
                        if passed { "passed" } else { "failed" }
                    ),
                );
                job.tests.push(TestAttempt {
                    attempt,
                    passed,
                    summary,
                });
            }
            Event::Log { level, message } => job.push(
                match level {
                    LogLevel::Warn => FeedKind::Warn,
                    _ => FeedKind::Info,
                },
                message,
            ),
            Event::StateChanged {
                run_id,
                state,
                step,
            } => {
                job.run_id = Some(run_id);
                job.state = state;
                job.set_step(step);
            }
            _ => {}
        }
    }

    /// Answers the open question (`None` ends or pauses the session).
    pub fn answer(&mut self, answer: Option<String>) {
        let Some(mut q) = self.question.take() else {
            return;
        };
        if let (Some(job), Some(text)) = (&mut self.job, &answer) {
            job.push(FeedKind::Answer, text.clone());
        }
        if let Some(reply) = q.reply.take() {
            let _ = reply.send(answer);
        }
    }

    pub fn decide(&mut self, decision: Decision) {
        let Some(mut a) = self.approval.take() else {
            return;
        };
        if let Some(job) = &mut self.job {
            let verb = match decision {
                Decision::Approve => "Approved",
                Decision::Reject => "Rejected",
            };
            job.push(FeedKind::Info, format!("{verb}: {}", a.summary));
        }
        if let Some(reply) = a.reply.take() {
            let _ = reply.send(decision);
        }
    }
}

fn stage_title(stage: Stage) -> String {
    let s = stage.to_string();
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => s,
    }
}

fn stage_step(stage: Stage) -> Option<RunStep> {
    Some(match stage {
        Stage::Plan => RunStep::Plan,
        Stage::Work => RunStep::Work,
        Stage::Test => RunStep::Test,
        Stage::Document => RunStep::Document,
        Stage::WriteBack => RunStep::WriteBack,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Diffs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Hunk,
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Relative to the project root when possible.
    pub path: String,
    pub status: FileStatus,
    pub added: usize,
    pub removed: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
}

fn pending_approval(
    request: ApprovalRequest,
    reply: oneshot::Sender<Decision>,
    project_root: &Path,
) -> PendingApproval {
    let files: Vec<FileDiff> = request
        .changes
        .iter()
        .map(|c| file_diff(c, project_root))
        .collect();
    let spec_diff = match (&request.diff, files.is_empty()) {
        (Some(d), true) => Some(parse_unified(d)),
        _ => None,
    };
    PendingApproval {
        kind: request.kind,
        summary: request.summary,
        files,
        spec_diff,
        reply: Some(reply),
    }
}

/// A unified diff (3 lines of context) of one file change.
pub fn file_diff(change: &FileChangePreview, project_root: &Path) -> FileDiff {
    let old = change.old_text.as_deref().unwrap_or("");
    let new = change.new_text.as_deref().unwrap_or("");
    let status = match (&change.old_text, &change.new_text) {
        (None, _) => FileStatus::Added,
        (_, None) => FileStatus::Deleted,
        _ => FileStatus::Modified,
    };
    let diff = TextDiff::from_lines(old, new);
    let mut lines = Vec::new();
    let (mut added, mut removed) = (0, 0);
    for group in diff.grouped_ops(3) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let (o, n) = (first.old_range().start, first.new_range().start);
        let (o_len, n_len) = (last.old_range().end - o, last.new_range().end - n);
        lines.push(DiffLine {
            kind: LineKind::Hunk,
            text: format!("@@ -{},{o_len} +{},{n_len} @@", o + 1, n + 1),
        });
        for op in &group {
            for change in diff.iter_changes(op) {
                let kind = match change.tag() {
                    ChangeTag::Equal => LineKind::Context,
                    ChangeTag::Insert => {
                        added += 1;
                        LineKind::Added
                    }
                    ChangeTag::Delete => {
                        removed += 1;
                        LineKind::Removed
                    }
                };
                lines.push(DiffLine {
                    kind,
                    text: change.value().trim_end_matches(['\n', '\r']).to_string(),
                });
            }
        }
    }
    FileDiff {
        path: relative(&change.path, project_root),
        status,
        added,
        removed,
        lines,
    }
}

/// Classifies the lines of an existing unified diff (the spec diff).
pub fn parse_unified(diff: &str) -> Vec<DiffLine> {
    diff.lines()
        .filter(|l| !l.starts_with("+++") && !l.starts_with("---"))
        .map(|l| {
            let (kind, text) = match l.chars().next() {
                Some('@') => (LineKind::Hunk, l),
                Some('+') => (LineKind::Added, &l[1..]),
                Some('-') => (LineKind::Removed, &l[1..]),
                Some(' ') => (LineKind::Context, &l[1..]),
                _ => (LineKind::Context, l),
            };
            DiffLine {
                kind,
                text: text.to_string(),
            }
        })
        .collect()
}

fn relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// Feature specs on disk
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureEntry {
    pub name: String,
    /// `approved`, `draft`, or `unknown`.
    pub status: String,
    pub path: PathBuf,
}

/// Feature specs in `specs_dir`: `<feature>.md` files with a `status:` line in
/// their front matter. `README.md` is not a feature.
pub fn list_features(specs_dir: &Path) -> Vec<FeatureEntry> {
    let Ok(entries) = std::fs::read_dir(specs_dir) else {
        return Vec::new();
    };
    let mut out: Vec<FeatureEntry> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .filter_map(|path| {
            let name = path.file_stem()?.to_str()?.to_string();
            if name.eq_ignore_ascii_case("readme") {
                return None;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            Some(FeatureEntry {
                name,
                status: front_matter_status(&text).unwrap_or("unknown").to_string(),
                path,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn front_matter_status(text: &str) -> Option<&str> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    lines
        .take_while(|l| l.trim() != "---")
        .find_map(|l| l.trim().strip_prefix("status:"))
        .map(str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Finished;

    fn state_with_build() -> AppState {
        let mut s = AppState::default();
        s.start_job(JobKind::Build, "build auth");
        s
    }

    #[test]
    fn steps_follow_run_state() {
        let mut s = state_with_build();
        let root = Path::new("/p");
        s.apply(
            ToUi::Event(Event::StateChanged {
                run_id: "r1".into(),
                state: "working (attempt 1)".into(),
                step: RunStep::Work,
            }),
            root,
        );
        let job = s.job.as_ref().unwrap();
        assert_eq!(job.run_id.as_deref(), Some("r1"));
        assert_eq!(job.state, "working (attempt 1)");
        assert_eq!(
            job.steps,
            [
                StepState::Done,
                StepState::Active,
                StepState::Pending,
                StepState::Pending,
                StepState::Pending
            ]
        );
        s.apply(
            ToUi::Finished(Finished {
                ok: false,
                message: "tests still failing".into(),
                exit_code: 7,
            }),
            root,
        );
        let job = s.job.as_ref().unwrap();
        assert_eq!(job.steps[1], StepState::Failed);
        assert!(!s.busy());
        assert_eq!(job.feed.last().unwrap().kind, FeedKind::Warn);
    }

    #[test]
    fn done_marks_every_step() {
        let mut s = state_with_build();
        s.apply(
            ToUi::Event(Event::StateChanged {
                run_id: "r".into(),
                state: "done".into(),
                step: RunStep::Done,
            }),
            Path::new("/"),
        );
        assert_eq!(s.job.unwrap().steps, [StepState::Done; 5]);
    }

    #[test]
    fn feed_collects_agents_tools_and_tests() {
        let mut s = state_with_build();
        let root = Path::new("/");
        for e in [
            Event::StageStarted { stage: Stage::Plan },
            Event::AgentMessage {
                agent: AgentName::Planner,
                text: "3 steps".into(),
            },
            Event::ToolUse {
                agent: AgentName::Worker,
                description: "grep fn main".into(),
            },
            Event::TestResult {
                attempt: 1,
                passed: false,
                summary: "1 failed".into(),
            },
        ] {
            s.apply(ToUi::Event(e), root);
        }
        let job = s.job.unwrap();
        let kinds: Vec<_> = job.feed.iter().map(|f| f.kind.clone()).collect();
        assert_eq!(
            kinds,
            [
                FeedKind::Stage,
                FeedKind::Agent(AgentName::Planner),
                FeedKind::Tool(AgentName::Worker),
                FeedKind::Test { passed: false }
            ]
        );
        assert_eq!(job.feed[0].text, "Plan");
        assert_eq!(job.steps[0], StepState::Active);
        assert_eq!(job.tests[0].summary, "1 failed");
    }

    #[test]
    fn events_without_a_job_are_ignored() {
        let mut s = AppState::default();
        s.apply(
            ToUi::Event(Event::StageStarted { stage: Stage::Plan }),
            Path::new("/"),
        );
        assert!(s.job.is_none());
    }

    #[tokio::test]
    async fn answering_replies_and_records_the_answer() {
        let mut s = AppState::default();
        s.start_job(JobKind::Spec, "spec auth");
        let (reply, rx) = oneshot::channel();
        s.apply(
            ToUi::Question {
                text: "Who?".into(),
                reply,
            },
            Path::new("/"),
        );
        assert_eq!(s.question.as_ref().unwrap().text, "Who?");
        s.answer(Some("admins".into()));
        assert!(s.question.is_none());
        assert_eq!(rx.await.unwrap().as_deref(), Some("admins"));
        let feed = &s.job.unwrap().feed;
        assert_eq!(feed[0].kind, FeedKind::Question);
        assert_eq!(feed[1].kind, FeedKind::Answer);
    }

    #[tokio::test]
    async fn approval_decision_is_sent_back() {
        let mut s = state_with_build();
        let (reply, rx) = oneshot::channel();
        let root = PathBuf::from("/proj");
        s.apply(
            ToUi::Approval {
                request: ApprovalRequest {
                    kind: ApprovalKind::WriteBack,
                    summary: "1 file".into(),
                    diff: None,
                    changes: vec![FileChangePreview {
                        path: root.join("src/lib.rs"),
                        old_text: Some("a\nb\n".into()),
                        new_text: Some("a\nc\n".into()),
                    }],
                },
                reply,
            },
            &root,
        );
        let a = s.approval.as_ref().unwrap();
        assert_eq!(a.title(), "Write changes to your project?");
        assert_eq!(a.files[0].path, "src/lib.rs");
        s.decide(Decision::Approve);
        assert_eq!(rx.await.unwrap(), Decision::Approve);
        assert!(s.approval.is_none());
    }

    #[tokio::test]
    async fn finishing_drops_open_prompts_which_rejects() {
        let mut s = state_with_build();
        let (reply, rx) = oneshot::channel();
        s.apply(
            ToUi::Approval {
                request: ApprovalRequest {
                    kind: ApprovalKind::WriteBack,
                    summary: "x".into(),
                    diff: None,
                    changes: vec![],
                },
                reply,
            },
            Path::new("/"),
        );
        s.apply(
            ToUi::Finished(Finished {
                ok: false,
                message: "cancelled".into(),
                exit_code: 130,
            }),
            Path::new("/"),
        );
        assert!(s.approval.is_none());
        assert!(rx.await.is_err());
    }

    #[test]
    fn file_diff_counts_and_hunks() {
        let root = Path::new("/p");
        let d = file_diff(
            &FileChangePreview {
                path: root.join("a.txt"),
                old_text: Some("1\n2\n3\n".into()),
                new_text: Some("1\ntwo\n3\n4\n".into()),
            },
            root,
        );
        assert_eq!(d.path, "a.txt");
        assert_eq!(d.status, FileStatus::Modified);
        assert_eq!((d.added, d.removed), (2, 1));
        assert_eq!(d.lines[0].kind, LineKind::Hunk);
        assert!(
            d.lines
                .iter()
                .any(|l| l.kind == LineKind::Removed && l.text == "2")
        );
        assert!(
            d.lines
                .iter()
                .any(|l| l.kind == LineKind::Added && l.text == "two")
        );

        let new_file = file_diff(
            &FileChangePreview {
                path: root.join("n.txt"),
                old_text: None,
                new_text: Some("x\n".into()),
            },
            root,
        );
        assert_eq!(new_file.status, FileStatus::Added);
        assert_eq!((new_file.added, new_file.removed), (1, 0));
    }

    #[test]
    fn unified_diff_is_classified() {
        let lines = parse_unified("--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n same\n");
        let kinds: Vec<_> = lines.iter().map(|l| l.kind).collect();
        assert_eq!(
            kinds,
            [
                LineKind::Hunk,
                LineKind::Removed,
                LineKind::Added,
                LineKind::Context
            ]
        );
        assert_eq!(lines[1].text, "old");
    }

    #[test]
    fn features_are_listed_with_status() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "# specs").unwrap();
        std::fs::write(
            dir.path().join("auth.md"),
            "---\nstatus: approved\n---\n# Auth\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("billing.md"), "---\nstatus: draft\n---\n").unwrap();
        std::fs::write(dir.path().join("notes.md"), "no front matter").unwrap();
        std::fs::write(dir.path().join("x.txt"), "").unwrap();
        let f = list_features(dir.path());
        let got: Vec<_> = f
            .iter()
            .map(|e| (e.name.as_str(), e.status.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("auth", "approved"),
                ("billing", "draft"),
                ("notes", "unknown")
            ]
        );
        assert!(list_features(&dir.path().join("missing")).is_empty());
    }
}
