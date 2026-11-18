//! Orchestration. Sequences pipelines, decides retries and write-back, maps
//! failures to exit codes. Never prints, reads input, or speaks a protocol.

mod channel;
mod error;
mod graph;
mod pipeline;
mod runs;
mod session;
mod skills;
mod state;
mod tools;
mod workspace;

pub use channel::{
    AgentName, ApprovalKind, ApprovalRequest, Channel, Decision, Event, FileChangePreview,
    LogLevel, Question, RunStep, Stage,
};
pub use error::Error;
pub use graph::{GraphQuery, explore_graph};
pub use pipeline::{BuildOutcome, BuildRequest};
pub use runs::{CancelToken, RunSummary, clean_runs, discard_run, list_runs};
pub use session::{SpecOutcome, SpecRequest};
pub use skills::{SkillStatus, check_skills};

use std::path::Path;
use std::sync::Arc;

use crate::agents::{AgentModels, Agents, ToolLimits};
use crate::config::{Config, SandboxBackend};
use crate::index::Scope;
use crate::llm::Llm;
use crate::sandbox::{ContainerSandbox, ProcessSandbox, Sandbox};

pub struct Engine {
    config: Config,
    llm: Arc<dyn Llm>,
    models: AgentModels,
    sandbox: Arc<dyn Sandbox>,
}

impl Engine {
    /// Builds an engine from configuration and a model client.
    pub fn new(config: Config, llm: Arc<dyn Llm>) -> Result<Self, Error> {
        let m = &config.models;
        let models = AgentModels {
            analyst: m.analyst.clone(),
            architect: m.architect.clone(),
            planner: m.planner.clone(),
            worker: m.worker.clone(),
            tester: m.tester.clone(),
            documentor: m.documentor.clone(),
            effort: (!m.effort.is_empty()).then(|| m.effort.clone()),
        };
        let sandbox: Arc<dyn Sandbox> = match config.sandbox.backend {
            SandboxBackend::Process => Arc::new(ProcessSandbox),
            SandboxBackend::Container => Arc::new(ContainerSandbox),
        };
        Ok(Self {
            config,
            llm,
            models,
            sandbox,
        })
    }

    /// Agents for one run, each with the skills the project requires for it.
    /// Fails if a required skill is not installed.
    fn agents(
        &self,
        project_root: &Path,
        ch: &dyn Channel,
    ) -> Result<(Agents, std::collections::BTreeMap<String, Vec<String>>), Error> {
        let resolved = skills::resolve(&self.config, project_root)?;
        for (agent, names) in &resolved.summary {
            ch.event(Event::Log {
                level: LogLevel::Info,
                message: format!("{agent} skills: {}", names.join(", ")),
            });
        }
        let limits = ToolLimits {
            max_calls: self.config.context.max_tool_calls,
            max_bytes: self.config.context.max_read_bytes,
        };
        let agents = Agents::new(
            self.llm.clone(),
            self.models.clone(),
            resolved.prompts,
            limits,
        );
        Ok((agents, resolved.summary))
    }

    /// The files agents may see (`[context] include/exclude`).
    fn scope(&self) -> Result<Scope, Error> {
        let c = &self.config.context;
        Scope::new(&c.include, &c.exclude).map_err(|e| Error::Config(format!("[context] {e}")))
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// `build <feature> "<task>"`.
    pub async fn build(
        &self,
        request: BuildRequest,
        channel: &dyn Channel,
    ) -> Result<BuildOutcome, Error> {
        pipeline::run(self, &request, channel).await
    }

    /// `spec <feature>`: start or resume a spec session.
    pub async fn spec_session(
        &self,
        request: SpecRequest,
        channel: &dyn Channel,
    ) -> Result<SpecOutcome, Error> {
        session::run(self, &request, channel).await
    }
}
