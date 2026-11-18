//! Resolves `[skills.required]` from config to installed `SKILL.md` files and
//! renders each agent's skill block. Missing skills fail the run before any
//! model call.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::agents::AgentRole;
use crate::config::Config;
use crate::skills::{self, Skill, SkillRef};

use super::Error;

/// One required skill and where it was found. For `kamili skills`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SkillStatus {
    /// Config key the requirement came from: `all`, `worker`, …
    pub agent: String,
    /// As written in config: `name` or `owner/repo@name`.
    pub reference: String,
    pub name: String,
    /// skills.sh source (`owner/repo`), when the reference names one.
    pub source: Option<String>,
    /// The `SKILL.md` that satisfies it, or `None` if not installed.
    pub found: Option<PathBuf>,
}

impl SkillStatus {
    /// The skills.sh command that installs this skill into the project.
    pub fn install_command(&self) -> Option<String> {
        self.source
            .as_ref()
            .map(|src| format!("npx skills add {src} --skill {} -y", self.name))
    }
}

/// Skills per agent for one run, ready to hand to `Agents`.
pub(crate) struct ResolvedSkills {
    pub prompts: HashMap<AgentRole, String>,
    /// Agent name → skill names, for logs and run reports.
    pub summary: BTreeMap<String, Vec<String>>,
}

fn config_key(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Analyst => "analyst",
        AgentRole::Architect => "architect",
        AgentRole::Planner => "planner",
        AgentRole::Worker => "worker",
        AgentRole::Tester => "tester",
        AgentRole::Documentor => "documentor",
    }
}

fn required_for<'a>(config: &'a Config, key: &str) -> &'a [String] {
    let r = &config.skills.required;
    match key {
        "all" => &r.all,
        "analyst" => &r.analyst,
        "architect" => &r.architect,
        "planner" => &r.planner,
        "worker" => &r.worker,
        "tester" => &r.tester,
        "documentor" => &r.documentor,
        _ => &[],
    }
}

/// `~/x` → `$HOME/x`; relative paths are under the project root.
fn search_dirs(config: &Config, root: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    config
        .skills
        .dirs
        .iter()
        .filter_map(|d| match d.strip_prefix("~/") {
            Some(rest) => home.as_ref().map(|h| h.join(rest)),
            None => Some(root.join(d)),
        })
        .collect()
}

/// Every required skill and whether it is installed.
pub fn check_skills(config: &Config, project_root: &Path) -> Result<Vec<SkillStatus>, Error> {
    Ok(scan(config, project_root)?.0)
}

/// Required skills with their status, plus every installed skill.
fn scan(config: &Config, project_root: &Path) -> Result<(Vec<SkillStatus>, Vec<Skill>), Error> {
    let installed = skills::discover(&search_dirs(config, project_root))
        .map_err(|e| Error::Config(e.to_string()))?;
    let keys = std::iter::once("all").chain(AgentRole::ALL.into_iter().map(config_key));
    let mut out = Vec::new();
    for key in keys {
        for reference in required_for(config, key) {
            let r = SkillRef::parse(reference)
                .map_err(|e| Error::Config(format!("[skills.required] {key}: {e}")))?;
            out.push(SkillStatus {
                agent: key.to_string(),
                reference: reference.clone(),
                found: installed
                    .iter()
                    .find(|s| s.name == r.name)
                    .map(|s| s.path.clone()),
                name: r.name,
                source: r.source,
            });
        }
    }
    Ok((out, installed))
}

pub(crate) fn resolve(config: &Config, project_root: &Path) -> Result<ResolvedSkills, Error> {
    let (statuses, installed) = scan(config, project_root)?;
    let missing: Vec<&SkillStatus> = statuses.iter().filter(|s| s.found.is_none()).collect();
    if !missing.is_empty() {
        let mut msg = String::from("required skills are not installed:");
        for s in &missing {
            msg.push_str(&format!("\n  {} ({})", s.reference, s.agent));
            if let Some(cmd) = s.install_command() {
                msg.push_str(&format!(": {cmd}"));
            }
        }
        msg.push_str("\nrun `kamili skills install`, or check [skills] dirs in .kamili.toml");
        return Err(Error::Config(msg));
    }
    if statuses.is_empty() {
        return Ok(ResolvedSkills {
            prompts: HashMap::new(),
            summary: BTreeMap::new(),
        });
    }

    let lookup = |name: &str| installed.iter().find(|s| s.name == name).cloned();
    let mut prompts = HashMap::new();
    let mut summary = BTreeMap::new();
    for role in AgentRole::ALL {
        let mut list: Vec<Skill> = Vec::new();
        for s in statuses
            .iter()
            .filter(|s| s.agent == "all" || s.agent == config_key(role))
        {
            if !list.iter().any(|k| k.name == s.name)
                && let Some(skill) = lookup(&s.name)
            {
                list.push(skill);
            }
        }
        if list.is_empty() {
            continue;
        }
        let text = skills::render(&list);
        if text.len() > config.skills.max_bytes {
            return Err(Error::Config(format!(
                "skills for the {} add {} bytes to its prompt, over [skills] max_bytes = {}",
                role.name(),
                text.len(),
                config.skills.max_bytes
            )));
        }
        summary.insert(
            role.name().to_string(),
            list.iter().map(|s| s.name.clone()).collect(),
        );
        prompts.insert(role, text);
    }
    Ok(ResolvedSkills { prompts, summary })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_with_skill(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join(".agents/skills").join(name);
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n---\nDo {name}."),
        )
        .unwrap();
        dir
    }

    #[test]
    fn all_plus_role_skills_reach_the_right_agents() {
        let dir = project_with_skill("tdd");
        let extra = dir.path().join(".agents/skills/rust");
        std::fs::create_dir_all(&extra).unwrap();
        std::fs::write(extra.join("SKILL.md"), "---\nname: rust\n---\nRust rules.").unwrap();
        let mut config = Config::default();
        config.skills.required.all = vec!["tdd".into()];
        config.skills.required.worker = vec!["o/r@rust".into(), "tdd".into()];

        let r = resolve(&config, dir.path()).unwrap();
        assert_eq!(r.summary["Worker"], ["tdd", "rust"]);
        assert_eq!(r.summary["Analyst"], ["tdd"]);
        assert!(r.prompts[&AgentRole::Worker].contains("Rust rules."));
        assert!(!r.prompts[&AgentRole::Tester].contains("Rust rules."));
    }

    #[test]
    fn missing_skill_fails_with_install_hint() {
        let dir = project_with_skill("tdd");
        let mut config = Config::default();
        config.skills.required.architect = vec!["vercel-labs/agent-skills@web-design".into()];
        let err = resolve(&config, dir.path()).err().unwrap().to_string();
        assert!(
            err.contains("npx skills add vercel-labs/agent-skills --skill web-design -y"),
            "{err}"
        );
    }

    #[test]
    fn size_cap_is_enforced() {
        let dir = project_with_skill("tdd");
        let mut config = Config::default();
        config.skills.required.all = vec!["tdd".into()];
        config.skills.max_bytes = 10;
        assert!(resolve(&config, dir.path()).is_err());
    }

    #[test]
    fn no_requirements_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let r = resolve(&Config::default(), dir.path()).unwrap();
        assert!(r.prompts.is_empty());
    }
}
