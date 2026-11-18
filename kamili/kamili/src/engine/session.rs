//! Spec session: Analyst ⇄ user interview, then a draft the user approves
//! (ARCHITECT.md §6). The sandbox is never involved and no code runs.

use std::path::PathBuf;

use crate::agents::{Agents, AnalystTurn, Workspace};
use crate::llm::{Message, Role};
use crate::spec::{self, Editor, SectionEdit, Spec, Status};

use super::pipeline::spec_label;
use super::tools::{ProjectTools, focused_map, load_graph_cache, save_graph_cache};
use super::workspace;
use super::{
    AgentName, ApprovalKind, ApprovalRequest, Channel, Decision, Engine, Error, Event,
    FileChangePreview, Question, Stage,
};

/// Max related specs and repo files included in the Analyst's context.
const MAX_CONTEXT_FILES: usize = 300;

#[derive(Debug, Clone)]
pub struct SpecRequest {
    pub project_root: PathBuf,
    pub feature: String,
    /// Continue from the transcript saved by an earlier session.
    pub resume: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpecOutcome {
    /// The spec was written and marked approved.
    Approved { path: PathBuf },
    /// The user rejected the draft; nothing was written.
    Rejected,
    /// The user stopped answering; the transcript was saved for `--resume`.
    Abandoned,
}

pub(crate) async fn run(
    engine: &Engine,
    req: &SpecRequest,
    ch: &dyn Channel,
) -> Result<SpecOutcome, Error> {
    let specs_dir = req.project_root.join(&engine.config.specs_dir);
    let path = spec::locate(&specs_dir, &req.feature)?;
    let existing = spec::load_optional(&specs_dir, &req.feature)?;
    let (agents, _) = engine.agents(&req.project_root, ch)?;

    let saved = if req.resume {
        workspace::read_record::<Vec<Message>>(&req.project_root, "sessions", &req.feature)
    } else {
        None
    };
    let mut transcript = match saved {
        Some(t) if !t.is_empty() => t,
        _ => vec![Message::user(context(
            req,
            &specs_dir,
            existing.as_ref().map(|(_, text)| text.as_str()),
        )?)],
    };

    ch.event(Event::StageStarted {
        stage: Stage::Interview,
    });

    // A session abandoned mid-interview ends on the Analyst's unanswered
    // question. Re-ask it so the transcript ends on a user turn again.
    if let Some(pending) = transcript
        .last()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.content.clone())
    {
        ch.event(Event::AgentMessage {
            agent: AgentName::Analyst,
            text: pending.clone(),
        });
        let Some(answer) = ch.ask(&Question { text: pending }).await else {
            return Ok(SpecOutcome::Abandoned);
        };
        transcript.push(Message::user(answer));
        save(req, &transcript)?;
    }

    let max_rounds = engine.config.session.max_rounds.max(1);
    for round in 1..=max_rounds {
        let turn = agents
            .analyst_turn(&transcript, round == max_rounds)
            .await?;
        match turn {
            AnalystTurn::Question { text } => {
                transcript.push(Message::assistant(text.clone()));
                ch.event(Event::AgentMessage {
                    agent: AgentName::Analyst,
                    text: text.clone(),
                });
                let Some(answer) = ch.ask(&Question { text }).await else {
                    save(req, &transcript)?;
                    return Ok(SpecOutcome::Abandoned);
                };
                transcript.push(Message::user(answer));
                save(req, &transcript)?;
            }
            AnalystTurn::Draft { spec: markdown } => {
                transcript.push(Message::assistant(markdown.clone()));
                save(req, &transcript)?;
                ch.event(Event::StageFinished {
                    stage: Stage::Interview,
                });
                let mut draft = spec::from_draft(&path, &markdown);
                if engine.config.session.architect {
                    draft = design(engine, &agents, req, ch, draft).await?;
                }
                return finalize(
                    req,
                    ch,
                    &path,
                    existing.as_ref().map(|(_, t)| t.as_str()),
                    draft,
                )
                .await;
            }
        }
    }
    save(req, &transcript)?;
    Err(Error::Agent(format!(
        "Analyst produced no draft within {max_rounds} rounds"
    )))
}

/// The Architect writes the draft's `Architecture` section.
async fn design(
    engine: &Engine,
    agents: &Agents,
    req: &SpecRequest,
    ch: &dyn Channel,
    draft: Spec,
) -> Result<Spec, Error> {
    ch.event(Event::StageStarted {
        stage: Stage::Design,
    });
    let cfg = &engine.config;
    let scope = engine.scope()?;
    let mut cache = load_graph_cache(&req.project_root);
    let (map, graph) = focused_map(
        &req.project_root,
        &scope,
        cfg.context.repo_map_bytes,
        &draft.render(),
        &[],
        &mut cache,
    );
    save_graph_cache(&req.project_root, &cache);
    let mut tools = ProjectTools::read_only(
        &req.project_root,
        &scope,
        &cfg.specs_dir,
        ch,
        AgentName::Architect,
    )
    .with_graph(graph, cache);
    let ws = Workspace {
        repo_map: &map,
        host: &mut tools,
    };
    let proposal = agents.design(&draft.render(), Some(ws)).await?;
    ch.event(Event::AgentMessage {
        agent: AgentName::Architect,
        text: proposal.summary.clone(),
    });
    let edits: Vec<SectionEdit> = proposal
        .section_edits
        .into_iter()
        .map(|e| SectionEdit {
            section: e.section,
            content: e.content,
        })
        .collect();
    let designed = draft.with_edits(Editor::Architect, &edits)?;
    ch.event(Event::StageFinished {
        stage: Stage::Design,
    });
    Ok(designed)
}

async fn finalize(
    req: &SpecRequest,
    ch: &dyn Channel,
    path: &std::path::Path,
    on_disk: Option<&str>,
    mut draft: Spec,
) -> Result<SpecOutcome, Error> {
    ch.event(Event::StageStarted {
        stage: Stage::Draft,
    });
    draft.status = Status::Approved; // what will be written if approved
    let label = spec_label(&req.project_root, path);
    let diff = spec::diff(on_disk.unwrap_or(""), &draft.render(), &label);
    ch.event(Event::SpecDiff {
        feature: req.feature.clone(),
        diff: diff.clone(),
    });

    let request = ApprovalRequest {
        kind: ApprovalKind::WriteSpec,
        summary: format!("Write {label} and mark it approved"),
        diff: Some(diff),
        changes: vec![FileChangePreview {
            path: path.to_path_buf(),
            old_text: on_disk.map(str::to_string),
            new_text: Some(draft.render()),
        }],
    };
    let outcome = match ch.approve(&request).await {
        Decision::Approve => {
            draft.write_if_unchanged(on_disk)?;
            SpecOutcome::Approved {
                path: path.to_path_buf(),
            }
        }
        Decision::Reject => SpecOutcome::Rejected,
    };
    ch.event(Event::StageFinished {
        stage: Stage::Draft,
    });
    Ok(outcome)
}

fn save(req: &SpecRequest, transcript: &[Message]) -> Result<(), Error> {
    workspace::save_record(&req.project_root, "sessions", &req.feature, &transcript).map(|_| ())
}

/// Read-only context for the Analyst: existing spec, related specs, repo layout.
fn context(
    req: &SpecRequest,
    specs_dir: &std::path::Path,
    existing: Option<&str>,
) -> Result<String, Error> {
    let mut out = format!("Feature: {}\n\n", req.feature);
    match existing {
        Some(text) => out.push_str(&format!("<existing_spec>\n{text}\n</existing_spec>\n\n")),
        None => out.push_str("There is no existing spec; this is a new feature.\n\n"),
    }
    if specs_dir.is_dir() {
        let related: Vec<String> = workspace::list_files(specs_dir)?
            .into_iter()
            .filter(|f| f.ends_with(".md") && *f != format!("{}.md", req.feature))
            .take(MAX_CONTEXT_FILES)
            .collect();
        out.push_str(&format!(
            "<related_specs>\n{}\n</related_specs>\n\n",
            related.join("\n")
        ));
    }
    let mut files = workspace::list_files(&req.project_root)?;
    files.truncate(MAX_CONTEXT_FILES);
    out.push_str(&format!(
        "<repository_files>\n{}\n</repository_files>",
        files.join("\n")
    ));
    Ok(out)
}
