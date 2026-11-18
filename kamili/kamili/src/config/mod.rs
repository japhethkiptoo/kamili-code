//! Loads the optional `.kamili.toml` and merges defaults. Leaf module.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Config file name, looked up in the project root.
pub const FILE_NAME: &str = ".kamili.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where feature specs live, relative to the project root.
    pub specs_dir: PathBuf,
    pub provider: ProviderConfig,
    pub models: ModelConfig,
    pub build: BuildConfig,
    pub sandbox: SandboxConfig,
    pub session: SessionConfig,
    pub skills: SkillsConfig,
    pub context: ContextConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            specs_dir: PathBuf::from("docs/features"),
            provider: ProviderConfig::default(),
            models: ModelConfig::default(),
            build: BuildConfig::default(),
            sandbox: SandboxConfig::default(),
            session: SessionConfig::default(),
            skills: SkillsConfig::default(),
            context: ContextConfig::default(),
        }
    }
}

/// Which API the model IDs in `[models]` are sent to. The engine never reads
/// this; channels use it to pick a model client.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    /// Claude Messages API (credentials from `kamili login`).
    #[default]
    Anthropic,
    /// Any OpenAI-compatible Chat Completions API: NVIDIA NIM, OpenRouter,
    /// Groq, Ollama, vLLM, LM Studio, …
    Openai,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    /// API root including the version path, e.g.
    /// `https://integrate.api.nvidia.com/v1`. Required for `openai`; for
    /// `anthropic`, empty means `https://api.anthropic.com/v1`.
    pub base_url: String,
    /// Environment variable holding the API key for `openai`, e.g.
    /// `NVIDIA_API_KEY`. Empty = no key (local servers such as Ollama).
    /// Keys are never read from this file.
    pub api_key_env: String,
}

/// Model ID per agent role (ARCHITECT.md §5 tiers).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub analyst: String,
    pub architect: String,
    pub planner: String,
    pub worker: String,
    pub tester: String,
    pub documentor: String,
    /// Reasoning effort sent with every request (`low` … `max`). Empty = model default.
    pub effort: String,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            analyst: "claude-opus-5-5".into(),
            architect: "claude-opus-5-5".into(),
            planner: "claude-opus-5-5".into(),
            worker: "claude-sonnet-5-5".into(),
            tester: "claude-sonnet-5-5".into(),
            documentor: "claude-haiku-5-5".into(),
            effort: "high".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuildConfig {
    /// Command run inside the sandbox to test a working copy, e.g. `["cargo", "test"]`.
    /// Required for builds (not for `--docs-only`).
    pub test_command: Vec<String>,
    /// Extra Worker attempts after the first failure.
    pub max_retries: u32,
    /// Ask the channel before accepting the Planner's spec edits.
    pub approve_spec_edits: bool,
    /// Ask the channel before writing changes back to the real tree.
    pub approve_write_back: bool,
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            test_command: Vec::new(),
            max_retries: 3,
            approve_spec_edits: true,
            approve_write_back: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxBackend {
    /// Plain child process. NO isolation; development only until the container
    /// backend lands (ARCHITECT.md §11).
    Process,
    /// Container runtime with no network and resource limits. Not implemented yet.
    Container,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub backend: SandboxBackend,
    pub timeout_secs: u64,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            backend: SandboxBackend::Process,
            timeout_secs: 600,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// Maximum Analyst turns before a draft is forced.
    pub max_rounds: u32,
    /// Run the Architect on the Analyst's draft to write `## Architecture`.
    pub architect: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_rounds: 20,
            architect: true,
        }
    }
}

/// How agents see the codebase: a capped repo map, then read-only tools
/// (list, find, grep, read, outline) and, for the Worker, edit tools.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    /// Globs of files agents may see (empty = everything not ignored).
    pub include: Vec<String>,
    /// Globs of files agents never see, on top of `.gitignore` and `.kamiliignore`.
    pub exclude: Vec<String>,
    /// Size of the repo map given to the Architect, Planner and Worker, in bytes.
    pub repo_map_bytes: usize,
    /// Tool calls one agent may make in one turn.
    pub max_tool_calls: u32,
    /// Total tool output one agent may receive in one turn, in bytes.
    pub max_read_bytes: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            repo_map_bytes: 12_000,
            max_tool_calls: 60,
            max_read_bytes: 600_000,
        }
    }
}

/// Skills (`SKILL.md`, as installed by `npx skills add`) each agent must load.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    /// Where to look, in order; relative to the project root, `~/` for home.
    pub dirs: Vec<String>,
    /// Cap on the skill text added to one agent's prompt, in bytes.
    pub max_bytes: usize,
    pub required: RequiredSkills,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            dirs: vec![".agents/skills".into(), "~/.agents/skills".into()],
            max_bytes: 60_000,
            required: RequiredSkills::default(),
        }
    }
}

/// Required skills per agent: `name` or `owner/repo@name`. A run fails before
/// any model call if one is missing.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequiredSkills {
    /// Loaded by every agent.
    pub all: Vec<String>,
    pub analyst: Vec<String>,
    pub architect: Vec<String>,
    pub planner: Vec<String>,
    pub worker: Vec<String>,
    pub tester: Vec<String>,
    pub documentor: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

/// Loads `<project_root>/.kamili.toml`, or defaults if it does not exist.
pub fn load(project_root: &Path) -> Result<Config, ConfigError> {
    let path = project_root.join(FILE_NAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|source| ConfigError::Parse { path, source }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(source) => Err(ConfigError::Read { path, source }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_merges_with_defaults() {
        let cfg: Config =
            toml::from_str("[build]\ntest_command = [\"cargo\", \"test\"]\n").unwrap();
        assert_eq!(cfg.build.test_command, ["cargo", "test"]);
        assert_eq!(cfg.build.max_retries, 3);
        assert_eq!(cfg.models.analyst, "claude-opus-5-5");
    }

    #[test]
    fn skills_table_parses() {
        let cfg: Config =
            toml::from_str("[skills.required]\nall = [\"tdd\"]\nworker = [\"o/r@x\"]\n").unwrap();
        assert_eq!(cfg.skills.required.all, ["tdd"]);
        assert_eq!(cfg.skills.dirs[0], ".agents/skills");
        assert!(toml::from_str::<Config>("[skills.required]\nreviewer = []\n").is_err());
    }

    #[test]
    fn provider_defaults_to_anthropic_and_parses_openai() {
        let cfg = Config::default();
        assert_eq!(cfg.provider.kind, ProviderKind::Anthropic);
        assert!(cfg.provider.base_url.is_empty());
        let cfg: Config = toml::from_str(
            "[provider]\nkind = \"openai\"\nbase_url = \"https://integrate.api.nvidia.com/v1\"\napi_key_env = \"NVIDIA_API_KEY\"\n",
        )
        .unwrap();
        assert_eq!(cfg.provider.kind, ProviderKind::Openai);
        assert_eq!(cfg.provider.api_key_env, "NVIDIA_API_KEY");
        assert!(toml::from_str::<Config>("[provider]\nkind = \"gemini\"\n").is_err());
        assert!(toml::from_str::<Config>("[provider]\napi_key = \"x\"\n").is_err());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("[build]\nretries = 2\n").is_err());
    }
}
