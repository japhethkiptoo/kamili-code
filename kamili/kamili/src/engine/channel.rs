//! The Channel contract (ARCHITECT.md §7). Channels implement [`Channel`]; the
//! engine calls it. Enums are `#[non_exhaustive]` so new events and kinds can be
//! added without breaking channels.

use std::fmt;

use async_trait::async_trait;

#[async_trait]
pub trait Channel: Send + Sync {
    /// One-way progress. Must return quickly and never block the pipeline;
    /// buffer or drop if the sink is slow.
    fn event(&self, event: Event);

    /// Free-text question to the user. `None` means no answer: the user ended
    /// the session, or the channel is non-interactive.
    async fn ask(&self, question: &Question) -> Option<String>;

    /// Gate before something irreversible. Channels should default to
    /// [`Decision::Reject`] when unsure.
    async fn approve(&self, request: &ApprovalRequest) -> Decision;
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    StageStarted {
        stage: Stage,
    },
    StageFinished {
        stage: Stage,
    },
    AgentMessage {
        agent: AgentName,
        text: String,
    },
    SpecDiff {
        feature: String,
        diff: String,
    },
    TestResult {
        attempt: u32,
        passed: bool,
        summary: String,
    },
    Log {
        level: LogLevel,
        message: String,
    },
    /// A build run moved to a new state (see `kamili runs`).
    StateChanged {
        run_id: String,
        /// Human-readable state, e.g. `working (attempt 2)`.
        state: String,
        /// Which pipeline step the state belongs to.
        step: RunStep,
    },
    /// An agent called a tool, e.g. `grep "fn main" in src`.
    ToolUse {
        agent: AgentName,
        description: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stage {
    LoadSpec,
    Plan,
    Work,
    Test,
    Document,
    WriteBack,
    Interview,
    Design,
    Draft,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Stage::LoadSpec => "load spec",
            Stage::Plan => "plan",
            Stage::Work => "work",
            Stage::Test => "test",
            Stage::Document => "document",
            Stage::WriteBack => "write back",
            Stage::Interview => "interview",
            Stage::Design => "design",
            Stage::Draft => "draft",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentName {
    Analyst,
    Architect,
    Planner,
    Worker,
    Tester,
    Documentor,
}

impl fmt::Display for AgentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// The steps of a build, for progress displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunStep {
    Plan,
    Work,
    Test,
    Document,
    WriteBack,
    /// Finished successfully.
    Done,
    /// Failed or cancelled; see the state text.
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LogLevel {
    Info,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalKind {
    /// Accept the Planner's edits to an approved spec before building.
    ApplySpecEdits,
    /// Write a spec drafted in a spec session and mark it approved.
    WriteSpec,
    /// Write the build's changes back to the real tree.
    WriteBack,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub kind: ApprovalKind,
    pub summary: String,
    /// Unified diff of the spec change, when there is one.
    pub diff: Option<String>,
    /// Full before/after text of each file the approval would write (spec
    /// and code), for channels that can show real diffs. Binary and very
    /// large files are left out.
    pub changes: Vec<FileChangePreview>,
}

/// One file an approval would write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChangePreview {
    /// Absolute path in the real project.
    pub path: std::path::PathBuf,
    /// Current contents; `None` if the file doesn't exist yet.
    pub old_text: Option<String>,
    /// New contents; `None` if the file will be deleted.
    pub new_text: Option<String>,
}

impl FileChangePreview {
    /// Largest file included in previews.
    pub const MAX_BYTES: u64 = 256 * 1024;

    /// Preview for `rel`, comparing `real_root` with `new_root` (a working
    /// copy). `None` if either side is binary or too large.
    pub(crate) fn between(
        real_root: &std::path::Path,
        new_root: &std::path::Path,
        rel: &str,
    ) -> Option<Self> {
        let read = |p: std::path::PathBuf| -> Result<Option<String>, ()> {
            match std::fs::metadata(&p) {
                Err(_) => Ok(None),
                Ok(m) if m.len() > Self::MAX_BYTES => Err(()),
                Ok(_) => {
                    let bytes = std::fs::read(&p).map_err(|_| ())?;
                    String::from_utf8(bytes).map(Some).map_err(|_| ())
                }
            }
        };
        Some(Self {
            path: real_root.join(rel),
            old_text: read(real_root.join(rel)).ok()?,
            new_text: read(new_root.join(rel)).ok()?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Decision {
    Approve,
    #[default]
    Reject,
}
