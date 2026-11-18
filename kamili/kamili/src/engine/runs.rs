//! Build run records: `.kamili/runs/<id>/state.json` (the whole run, rewritten
//! on every transition) and `journal.jsonl` (one line per transition, append
//! only). Also listing, cleanup, and the cancel token.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::agents::RoleUsage;

use super::Error;
use super::state::BuildState;
use super::workspace::{self, WorkingCopy};

const RUNS_DIR: &str = ".kamili/runs";
const STATE_FILE: &str = "state.json";
const JOURNAL_FILE: &str = "journal.jsonl";
const FORMAT_VERSION: u32 = 1;

/// Everything about one build run. Saved before every action, so a resumed
/// run continues with exactly this data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Run {
    pub version: u32,
    pub id: String,
    pub feature: String,
    pub task: String,
    pub docs_only: bool,
    pub state: BuildState,
    pub created: u64,
    pub updated: u64,
    /// The spec file's text when the run started; write-back refuses to
    /// overwrite the spec if it no longer matches.
    pub spec_on_disk: String,
    /// The spec as revised so far (Planner, then Documentor).
    pub spec: String,
    pub plan_summary: Option<String>,
    /// The Planner's notes for the Worker.
    #[serde(default)]
    pub context_brief: String,
    pub attempts: Vec<AttemptRecord>,
    /// Feedback for the next Worker attempt.
    pub feedback: Option<String>,
    pub last_test_output: Option<String>,
    pub work_summary: Option<String>,
    pub copy: Option<WorkingCopy>,
    pub files_changed: Vec<String>,
    /// Unchanged files that use the changed code (from the code graph).
    #[serde(default)]
    pub impact: Vec<String>,
    pub spec_changed: bool,
    /// Agent → skills loaded.
    pub skills: BTreeMap<String, Vec<String>>,
    /// Agent → calls, tool calls and tokens, summed over the run.
    pub usage: BTreeMap<String, RoleUsage>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AttemptRecord {
    pub attempt: u32,
    pub worker_summary: String,
    pub passed: Option<bool>,
    pub feedback: Option<String>,
}

#[derive(Serialize)]
struct JournalEntry<'a> {
    t: u64,
    from: String,
    input: &'a str,
    to: String,
}

impl Run {
    pub fn new(
        id: String,
        feature: &str,
        task: &str,
        docs_only: bool,
        spec_on_disk: String,
    ) -> Self {
        let now = workspace::timestamp();
        Self {
            version: FORMAT_VERSION,
            id,
            feature: feature.to_string(),
            task: task.to_string(),
            docs_only,
            state: BuildState::Planning,
            created: now,
            updated: now,
            spec: spec_on_disk.clone(),
            spec_on_disk,
            plan_summary: None,
            context_brief: String::new(),
            attempts: Vec::new(),
            feedback: None,
            last_test_output: None,
            work_summary: None,
            copy: None,
            files_changed: Vec::new(),
            impact: Vec::new(),
            spec_changed: false,
            skills: BTreeMap::new(),
            usage: BTreeMap::new(),
            error: None,
        }
    }
}

fn runs_dir(root: &Path) -> PathBuf {
    root.join(RUNS_DIR)
}

pub(crate) fn state_path(root: &Path, id: &str) -> PathBuf {
    runs_dir(root).join(id).join(STATE_FILE)
}

/// A fresh, unused run id: `<unix-seconds>-<feature>`.
pub(crate) fn new_id(root: &Path, feature: &str) -> String {
    let base = format!("{}-{feature}", workspace::timestamp());
    let mut id = base.clone();
    let mut n = 1;
    while runs_dir(root).join(&id).exists() {
        n += 1;
        id = format!("{base}-{n}");
    }
    id
}

/// Writes `state.json` atomically (temp file, then rename).
pub(crate) fn save(root: &Path, run: &Run) -> Result<PathBuf, Error> {
    let path = state_path(root, &run.id);
    let dir = path.parent().expect("state file has a parent");
    fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(run).map_err(|e| Error::Workspace(e.to_string()))?;
    let tmp = dir.join(format!("{STATE_FILE}.tmp"));
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

pub(crate) fn journal(
    root: &Path,
    id: &str,
    from: &BuildState,
    input: &str,
    to: &BuildState,
) -> Result<(), Error> {
    let line = serde_json::to_string(&JournalEntry {
        t: workspace::timestamp(),
        from: from.to_string(),
        input,
        to: to.to_string(),
    })
    .map_err(|e| Error::Workspace(e.to_string()))?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(runs_dir(root).join(id).join(JOURNAL_FILE))?;
    writeln!(f, "{line}")?;
    Ok(())
}

pub(crate) fn load(root: &Path, id: &str) -> Result<Run, Error> {
    if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
        return Err(Error::Config(format!("invalid run id `{id}`")));
    }
    let path = state_path(root, id);
    let text = fs::read_to_string(&path)
        .map_err(|_| Error::Config(format!("no run `{id}`; see `kamili runs`")))?;
    let run: Run = serde_json::from_str(&text)
        .map_err(|e| Error::Workspace(format!("{}: {e}", path.display())))?;
    if run.version != FORMAT_VERSION {
        return Err(Error::Workspace(format!(
            "run `{id}` was saved by another version of Kamili (format {})",
            run.version
        )));
    }
    Ok(run)
}

/// All runs, newest first. Unreadable records are skipped.
pub(crate) fn all(root: &Path) -> Vec<Run> {
    let Ok(entries) = fs::read_dir(runs_dir(root)) else {
        return Vec::new();
    };
    let mut runs: Vec<Run> = entries
        .filter_map(Result::ok)
        .filter_map(|e| load(root, &e.file_name().to_string_lossy()).ok())
        .collect();
    runs.sort_by(|a, b| (b.created, &b.id).cmp(&(a.created, &a.id)));
    runs
}

/// `last` → the newest resumable run; anything else is a run id.
pub(crate) fn resolve_id(root: &Path, id: &str) -> Result<String, Error> {
    if id != "last" {
        return Ok(id.to_string());
    }
    all(root)
        .into_iter()
        .find(|r| r.state.resume_point().is_some())
        .map(|r| r.id)
        .ok_or_else(|| Error::Config("no resumable runs; see `kamili runs`".into()))
}

/// One line of `kamili runs`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunSummary {
    pub id: String,
    pub feature: String,
    pub task: String,
    /// Human-readable state, e.g. `working (attempt 2)`.
    pub state: String,
    pub resumable: bool,
    /// Unix seconds.
    pub updated: u64,
    /// Input plus output tokens over all agents.
    pub tokens: u64,
    pub report: PathBuf,
}

/// Build runs recorded in `project_root`, newest first.
pub fn list_runs(project_root: &Path) -> Vec<RunSummary> {
    all(project_root)
        .into_iter()
        .map(|r| RunSummary {
            report: state_path(project_root, &r.id),
            tokens: r
                .usage
                .values()
                .map(|u| u.tokens.input_tokens + u.tokens.output_tokens)
                .sum(),
            resumable: r.state.resume_point().is_some(),
            state: r.state.to_string(),
            updated: r.updated,
            id: r.id,
            feature: r.feature,
            task: r.task,
        })
        .collect()
}

/// Deletes the working copies of runs that can't be resumed. With `records`,
/// also deletes those runs' records. Returns how many runs were cleaned.
pub fn clean_runs(project_root: &Path, records: bool) -> Result<usize, Error> {
    let mut cleaned = 0;
    for run in all(project_root) {
        if run.state.resume_point().is_some() {
            continue;
        }
        let mut did = false;
        if let Some(copy) = &run.copy
            && copy.path().exists()
        {
            copy.remove();
            did = true;
        }
        if records {
            fs::remove_dir_all(runs_dir(project_root).join(&run.id))?;
            did = true;
        }
        cleaned += usize::from(did);
    }
    Ok(cleaned)
}

/// Deletes one run, finished or not: its working copy and its record.
pub fn discard_run(project_root: &Path, id: &str) -> Result<(), Error> {
    let id = resolve_id(project_root, id)?;
    let run = load(project_root, &id)?;
    if let Some(copy) = &run.copy {
        copy.remove();
    }
    fs::remove_dir_all(runs_dir(project_root).join(&id))?;
    Ok(())
}

/// Cancels a running build at its next safe point. Cheap to clone.
#[derive(Debug, Clone)]
pub struct CancelToken(Arc<tokio::sync::watch::Sender<bool>>);

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(tokio::sync::watch::Sender::new(false)))
    }

    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    pub(crate) async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }
}
