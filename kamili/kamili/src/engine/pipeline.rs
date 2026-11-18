//! Build runner: drives the state machine in `state.rs` (ARCHITECT.md §6).
//!
//! Each loop iteration saves the run, performs the current state's action,
//! turns the outcome into an `Input`, and steps to the next state. All the
//! side effects live here; all the decisions about what comes next live in
//! `state::step`.

use std::path::{Path, PathBuf};

use crate::agents::{Agents, SectionEditProposal, Workspace};
use crate::index::{CodeGraph, Scope};
use crate::sandbox::Limits;
use crate::spec::{self, Editor, SectionEdit, Status};

use super::runs::{self, AttemptRecord, CancelToken, Run};
use super::state::{BuildState, FailReason, Input, Policy, step};
use super::tools::{ProjectTools, focused_map, load_graph_cache, save_graph_cache};
use super::workspace::{self, WorkingCopy};
use super::{
    AgentName, ApprovalKind, ApprovalRequest, Channel, Decision, Engine, Error, Event,
    FileChangePreview, LogLevel,
};

/// Bytes of stdout/stderr (each) handed to the Tester.
const TEST_OUTPUT_TAIL: usize = 20_000;
/// Files listed as possibly affected at write-back.
const MAX_IMPACT: usize = 15;

#[derive(Debug, Clone, Default)]
pub struct BuildRequest {
    pub project_root: PathBuf,
    pub feature: String,
    pub task: String,
    /// Build even if the spec is a draft.
    pub force: bool,
    /// Run only Planner and Documentor; write only the spec.
    pub docs_only: bool,
    /// Continue a saved run (`<run-id>` or `last`) instead of starting one.
    /// `feature`, `task`, `force` and `docs_only` are then taken from the run.
    pub resume: Option<String>,
    /// Cancels the run at its next safe point; the run stays resumable.
    pub cancel: Option<CancelToken>,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BuildOutcome {
    pub run_id: String,
    /// Worker attempts made (0 for `--docs-only`).
    pub attempts: u32,
    pub files_changed: Vec<String>,
    pub spec_changed: bool,
    /// The run record (`.kamili/runs/<id>/state.json`).
    pub report_path: Option<PathBuf>,
}

pub(crate) async fn run(
    engine: &Engine,
    req: &BuildRequest,
    ch: &dyn Channel,
) -> Result<BuildOutcome, Error> {
    let root = &req.project_root;
    let run = match &req.resume {
        Some(id) => resume(root, id, ch)?,
        None => start(engine, req, ch)?,
    };
    // Skills are resolved on every start and resume, before any model call.
    let (agents, skills) = engine.agents(root, ch)?;
    let scope = engine.scope()?;
    let mut runner = Runner {
        engine,
        root,
        ch,
        agents,
        scope,
        policy: Policy {
            max_retries: engine.config.build.max_retries,
            docs_only: run.docs_only,
            approve_spec_edits: engine.config.build.approve_spec_edits,
            approve_write_back: engine.config.build.approve_write_back,
        },
        run,
        last_error: None,
    };
    runner.run.skills = skills;
    runner.drive(req.cancel.as_ref()).await
}

/// Loads the spec, checks it may be built, and records a new run.
fn start(engine: &Engine, req: &BuildRequest, ch: &dyn Channel) -> Result<Run, Error> {
    let cfg = &engine.config;
    let specs_dir = req.project_root.join(&cfg.specs_dir);
    let (spec, on_disk) = spec::load(&specs_dir, &req.feature)?;
    if spec.status != Status::Approved {
        if !req.force {
            return Err(Error::NotApproved(req.feature.clone()));
        }
        warn(
            ch,
            format!("building draft spec `{}` (--force)", req.feature),
        );
    }
    if !req.docs_only && cfg.build.test_command.is_empty() {
        return Err(Error::Config(
            "set [build] test_command in .kamili.toml".into(),
        ));
    }
    let id = runs::new_id(&req.project_root, &req.feature);
    Ok(Run::new(
        id,
        &req.feature,
        &req.task,
        req.docs_only,
        on_disk,
    ))
}

fn resume(root: &Path, id: &str, ch: &dyn Channel) -> Result<Run, Error> {
    let id = runs::resolve_id(root, id)?;
    let mut run = runs::load(root, &id)?;
    let Some(at) = run.state.resume_point() else {
        return Err(Error::Config(format!(
            "run `{id}` is {}; only unfinished, cancelled or retryable failed runs can be resumed",
            run.state
        )));
    };
    if let Some(copy) = &run.copy {
        copy.check_exists()?;
    }
    ch.event(Event::Log {
        level: LogLevel::Info,
        message: format!("resuming run {id} at: {at}"),
    });
    run.state = at;
    run.error = None;
    Ok(run)
}

struct Runner<'a> {
    engine: &'a Engine,
    root: &'a Path,
    ch: &'a dyn Channel,
    agents: Agents,
    scope: Scope,
    policy: Policy,
    run: Run,
    /// The error behind a `Failed` state, when it happened in this process.
    last_error: Option<Error>,
}

impl Runner<'_> {
    async fn drive(&mut self, cancel: Option<&CancelToken>) -> Result<BuildOutcome, Error> {
        if !self.run.docs_only && !self.engine.sandbox.isolated() {
            warn(
                self.ch,
                "sandbox backend `process` has no isolation (network and filesystem are reachable)"
                    .into(),
            );
        }
        let report = loop {
            self.run.updated = workspace::timestamp();
            let report = runs::save(self.root, &self.run)?;
            self.ch.event(Event::StateChanged {
                run_id: self.run.id.clone(),
                state: self.run.state.to_string(),
                step: self.run.state.step(),
            });
            if self.run.state.is_terminal() {
                break report;
            }

            let state = self.run.state.clone();
            let outcome = match cancel {
                Some(token) if state.cancellable() => {
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => Ok(Input::Cancel),
                        r = self.act(&state) => r,
                    }
                }
                _ => self.act(&state).await,
            };
            self.collect_usage();
            let input = self.input_from(outcome);
            let next = step(&state, input.clone(), &self.policy);
            runs::journal(self.root, &self.run.id, &state, &label(&input), &next)?;
            self.run.state = next;
        };

        if self.run.state.resume_point().is_none()
            && let Some(copy) = &self.run.copy
        {
            copy.remove();
        }
        let attempts = self.run.attempts.len() as u32;
        match &self.run.state {
            BuildState::Done => Ok(BuildOutcome {
                run_id: self.run.id.clone(),
                attempts,
                files_changed: self.run.files_changed.clone(),
                spec_changed: self.run.spec_changed,
                report_path: Some(report),
            }),
            BuildState::Cancelled { .. } => Err(Error::Cancelled(self.run.id.clone())),
            BuildState::Failed {
                reason,
                resume_from,
            } => {
                self.ch.event(Event::Log {
                    level: LogLevel::Info,
                    message: match resume_from {
                        Some(_) => format!(
                            "run report: {}\nresume with `kamili build --resume {}`",
                            report.display(),
                            self.run.id
                        ),
                        None => format!("run report: {}", report.display()),
                    },
                });
                Err(self.last_error.take().unwrap_or_else(|| match reason {
                    FailReason::Rejected { what } => Error::Rejected(what.clone()),
                    FailReason::TestsFailed { attempts } => Error::TestsFailed {
                        attempts: *attempts,
                    },
                    FailReason::Error { message, .. } => Error::Workspace(message.clone()),
                }))
            }
            other => Err(Error::Workspace(format!("internal: stopped while {other}"))),
        }
    }

    fn input_from(&mut self, outcome: Result<Input, Error>) -> Input {
        match outcome {
            Ok(input) => input,
            Err(e) => {
                let exit_code = e.exit_code();
                let input = Input::Failed {
                    message: e.to_string(),
                    // Infrastructure, config, model and agent-output failures
                    // are worth retrying; conflicts and rule violations aren't.
                    retryable: matches!(exit_code, 1..=5),
                    exit_code,
                };
                self.run.error = Some(e.to_string());
                self.last_error = Some(e);
                input
            }
        }
    }

    fn collect_usage(&mut self) {
        for (role, u) in self.agents.take_usage() {
            let total = self.run.usage.entry(role.name().to_string()).or_default();
            total.calls += u.calls;
            total.tool_calls += u.tool_calls;
            total.tokens += u.tokens;
        }
    }

    /// Performs the action of one state.
    async fn act(&mut self, state: &BuildState) -> Result<Input, Error> {
        match state {
            BuildState::Planning => self.plan().await,
            BuildState::AwaitingPlanApproval => {
                let diff = self.spec_diff();
                let summary = self.run.plan_summary.clone().unwrap_or_default();
                Ok(self
                    .ask(ApprovalKind::ApplySpecEdits, summary, Some(diff))
                    .await)
            }
            BuildState::Working { attempt } => self.work(*attempt).await,
            BuildState::Testing { attempt } => self.test(*attempt).await,
            BuildState::Diagnosing { attempt } => self.diagnose(*attempt).await,
            BuildState::Documenting => self.document().await,
            BuildState::AwaitingWriteApproval => {
                let diff = self.spec_diff();
                let summary = write_back_summary(
                    &self.run.files_changed,
                    self.run.spec_changed,
                    &self.run.impact,
                );
                Ok(self
                    .ask(
                        ApprovalKind::WriteBack,
                        summary,
                        (!diff.is_empty()).then_some(diff),
                    )
                    .await)
            }
            BuildState::WritingBack => self.write_back(),
            terminal => Err(Error::Workspace(format!(
                "internal: no action for {terminal}"
            ))),
        }
    }

    async fn plan(&mut self) -> Result<Input, Error> {
        let cfg = &self.engine.config;
        let focus = format!("{}\n{}", self.run.task, self.run.spec);
        let mut cache = load_graph_cache(self.root);
        let (map, graph) = focused_map(
            self.root,
            &self.scope,
            cfg.context.repo_map_bytes,
            &focus,
            &[],
            &mut cache,
        );
        save_graph_cache(self.root, &cache);
        let mut tools = ProjectTools::read_only(
            self.root,
            &self.scope,
            &cfg.specs_dir,
            self.ch,
            AgentName::Planner,
        )
        .with_graph(graph, cache);
        let ws = Workspace {
            repo_map: &map,
            host: &mut tools,
        };
        let plan = self
            .agents
            .plan(&self.run.spec, &self.run.task, Some(ws))
            .await?;
        self.ch.event(Event::AgentMessage {
            agent: AgentName::Planner,
            text: plan.summary.clone(),
        });
        let before = self.run.spec.clone();
        self.apply_spec_edits(Editor::Planner, &plan.section_edits)?;
        self.run.plan_summary = Some(plan.summary);
        self.run.context_brief = plan.context_brief;
        let diff = spec::diff(&before, &self.run.spec, &self.spec_label());
        if !diff.is_empty() {
            self.ch.event(Event::SpecDiff {
                feature: self.run.feature.clone(),
                diff,
            });
        }
        Ok(Input::Planned {
            spec_changed: before != self.run.spec,
        })
    }

    async fn work(&mut self, attempt: u32) -> Result<Input, Error> {
        let cfg = &self.engine.config;
        if self.run.copy.is_none() {
            let dir = workspace::work_dir_for(self.root, &self.run.id);
            self.run.copy = Some(WorkingCopy::create(self.root, &dir)?);
            // Save now so a crash during the Worker's turn keeps the copy.
            runs::save(self.root, &self.run)?;
        }
        let copy = self.run.copy.as_mut().expect("working copy exists");
        copy.check_exists()?;
        // Rank towards the task, the Planner's brief, and files already changed.
        // The project's cache serves the copy: unchanged files match by
        // contents. It is not saved back, since the copy holds unapproved edits.
        let focus = format!("{}\n{}", self.run.task, self.run.context_brief);
        let mut cache = load_graph_cache(self.root);
        let (map, graph) = focused_map(
            copy.path(),
            &self.scope,
            cfg.context.repo_map_bytes,
            &focus,
            &copy.changed_paths(),
            &mut cache,
        );
        let mut tools = ProjectTools::working_copy(
            copy,
            &self.scope,
            &cfg.specs_dir,
            self.ch,
            AgentName::Worker,
        )
        .with_graph(graph, cache);
        let ws = Workspace {
            repo_map: &map,
            host: &mut tools,
        };
        let work = self
            .agents
            .work(
                &self.run.spec,
                &self.run.task,
                &self.run.context_brief,
                self.run.feedback.as_deref(),
                Some(ws),
            )
            .await?;
        drop(tools);

        // Whole-file changes in the reply, if any, go through the same rules.
        let copy = self.run.copy.as_mut().expect("working copy exists");
        for change in &work.files {
            if Path::new(&change.path).starts_with(&cfg.specs_dir) {
                return Err(Error::Agent(format!(
                    "Worker may not edit specs (`{}`)",
                    change.path
                )));
            }
            copy.apply(&change.path, change.content.as_deref())?;
        }
        self.ch.event(Event::AgentMessage {
            agent: AgentName::Worker,
            text: work.summary.clone(),
        });
        self.run.attempts.retain(|a| a.attempt != attempt);
        self.run.attempts.push(AttemptRecord {
            attempt,
            worker_summary: work.summary.clone(),
            passed: None,
            feedback: None,
        });
        self.run.work_summary = Some(work.summary);
        Ok(Input::Worked)
    }

    async fn test(&mut self, attempt: u32) -> Result<Input, Error> {
        let cfg = &self.engine.config;
        let copy =
            self.run.copy.as_ref().ok_or_else(|| {
                Error::Workspace("internal: testing without a working copy".into())
            })?;
        copy.check_exists()?;
        let limits = Limits {
            timeout: std::time::Duration::from_secs(cfg.sandbox.timeout_secs),
        };
        let out = self
            .engine
            .sandbox
            .run(copy.path(), &cfg.build.test_command, &limits)
            .await?;
        let passed = out.success();
        if let Some(a) = self.run.attempts.iter_mut().find(|a| a.attempt == attempt) {
            a.passed = Some(passed);
        }
        self.ch.event(Event::TestResult {
            attempt,
            passed,
            summary: if passed {
                "tests passed".into()
            } else {
                "tests failed".into()
            },
        });
        if passed {
            Ok(Input::TestsPassed)
        } else {
            self.run.last_test_output = Some(out.condensed(TEST_OUTPUT_TAIL));
            Ok(Input::TestsFailed)
        }
    }

    async fn diagnose(&mut self, attempt: u32) -> Result<Input, Error> {
        let output = self.run.last_test_output.clone().unwrap_or_default();
        let fb = self.agents.test_feedback(&self.run.spec, &output).await?;
        self.ch.event(Event::AgentMessage {
            agent: AgentName::Tester,
            text: fb.feedback.clone(),
        });
        if let Some(a) = self.run.attempts.iter_mut().find(|a| a.attempt == attempt) {
            a.feedback = Some(fb.feedback.clone());
        }
        self.run.feedback = Some(fb.feedback);
        Ok(Input::Diagnosed)
    }

    async fn document(&mut self) -> Result<Input, Error> {
        self.run.files_changed = self
            .run
            .copy
            .as_ref()
            .map(WorkingCopy::changed_paths)
            .unwrap_or_default();
        // Other files that use what changed: shown at write-back approval.
        self.run.impact = match &self.run.copy {
            Some(copy) if !self.run.files_changed.is_empty() => {
                let mut cache = load_graph_cache(self.root);
                let graph = CodeGraph::build_cached(copy.path(), &self.scope, &mut cache);
                let mut hits = graph.impact(&self.run.files_changed);
                hits.truncate(MAX_IMPACT);
                hits.into_iter()
                    .map(|(path, via)| format!("{path} (uses {})", via.join(", ")))
                    .collect()
            }
            _ => Vec::new(),
        };
        let summary = self
            .run
            .work_summary
            .clone()
            .unwrap_or_else(|| "(docs only: no code changes)".into());
        let doc = self
            .agents
            .document(&self.run.spec, &self.run.task, &summary)
            .await?;
        self.ch.event(Event::AgentMessage {
            agent: AgentName::Documentor,
            text: doc.summary.clone(),
        });
        self.apply_spec_edits(Editor::Documentor, &doc.section_edits)?;
        self.run.spec_changed = self.run.spec != self.run.spec_on_disk;
        let nothing = self.run.files_changed.is_empty() && !self.run.spec_changed;
        if nothing {
            self.ch.event(Event::Log {
                level: LogLevel::Info,
                message: "nothing to write".into(),
            });
        }
        Ok(Input::Documented {
            nothing_to_write: nothing,
        })
    }

    /// Single final write-back, after conflict checks on everything touched.
    fn write_back(&mut self) -> Result<Input, Error> {
        let spec = spec::parse(&self.spec_path()?, &self.run.spec)?;
        if let Some(copy) = &self.run.copy {
            copy.check_conflicts()?;
        }
        if self.run.spec_changed {
            spec.check_unchanged(Some(&self.run.spec_on_disk))?;
        }
        if let Some(copy) = &self.run.copy {
            copy.write_back()?;
        }
        if self.run.spec_changed {
            spec.write_if_unchanged(Some(&self.run.spec_on_disk))?;
        }
        Ok(Input::Written)
    }

    async fn ask(&self, kind: ApprovalKind, summary: String, diff: Option<String>) -> Input {
        match self
            .ch
            .approve(&ApprovalRequest {
                kind,
                summary,
                diff,
                changes: self.previews(kind),
            })
            .await
        {
            Decision::Approve => Input::Approved,
            Decision::Reject => Input::Rejected,
        }
    }

    /// Before/after text of what this approval would write.
    fn previews(&self, kind: ApprovalKind) -> Vec<FileChangePreview> {
        let mut out = Vec::new();
        let spec_changed = self.run.spec != self.run.spec_on_disk;
        if spec_changed && let Ok(path) = self.spec_path() {
            out.push(FileChangePreview {
                path,
                old_text: Some(self.run.spec_on_disk.clone()),
                new_text: Some(self.run.spec.clone()),
            });
        }
        if kind == ApprovalKind::WriteBack
            && let Some(copy) = &self.run.copy
        {
            out.extend(
                self.run
                    .files_changed
                    .iter()
                    .filter_map(|f| FileChangePreview::between(self.root, copy.path(), f)),
            );
        }
        out
    }

    fn apply_spec_edits(
        &mut self,
        editor: Editor,
        proposals: &[SectionEditProposal],
    ) -> Result<(), Error> {
        let edits: Vec<SectionEdit> = proposals
            .iter()
            .map(|p| SectionEdit {
                section: p.section.clone(),
                content: p.content.clone(),
            })
            .collect();
        let current = spec::parse(&self.spec_path()?, &self.run.spec)?;
        self.run.spec = current.with_edits(editor, &edits)?.render();
        Ok(())
    }

    fn spec_path(&self) -> Result<PathBuf, Error> {
        let specs_dir = self.root.join(&self.engine.config.specs_dir);
        Ok(spec::locate(&specs_dir, &self.run.feature)?)
    }

    fn spec_label(&self) -> String {
        self.spec_path()
            .map(|p| spec_label(self.root, &p))
            .unwrap_or_else(|_| self.run.feature.clone())
    }

    fn spec_diff(&self) -> String {
        spec::diff(&self.run.spec_on_disk, &self.run.spec, &self.spec_label())
    }
}

fn label(input: &Input) -> String {
    match input {
        Input::Failed { message, .. } => format!("failed: {message}"),
        other => format!("{other:?}"),
    }
}

fn warn(ch: &dyn Channel, message: String) {
    ch.event(Event::Log {
        level: LogLevel::Warn,
        message,
    });
}

fn write_back_summary(files: &[String], spec_changed: bool, impact: &[String]) -> String {
    let mut s = format!("Write {} file(s) to the project", files.len());
    if spec_changed {
        s.push_str(" and update the spec");
    }
    for f in files {
        s.push_str(&format!("\n  {f}"));
    }
    if !impact.is_empty() {
        s.push_str("\nThese files use the changed code (unchanged; worth a look):");
        for f in impact {
            s.push_str(&format!("\n  {f}"));
        }
    }
    s
}

pub(crate) fn spec_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}
