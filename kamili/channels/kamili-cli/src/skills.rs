//! `kamili skills`: report required skills, and install missing ones with the
//! skills.sh CLI (`npx skills add`). The engine only reads installed skills.

use std::path::Path;
use std::process::Command;

use kamili::{Config, Error, SkillStatus, check_skills};

pub fn list(config: &Config, root: &Path) -> Result<(), Error> {
    let statuses = check_skills(config, root)?;
    if statuses.is_empty() {
        println!("no required skills; add them under [skills.required] in .kamili.toml");
        return Ok(());
    }
    for s in &statuses {
        match &s.found {
            Some(path) => {
                let shown = path.strip_prefix(root).unwrap_or(path);
                println!(
                    "ok       {:<11} {}  ({})",
                    s.agent,
                    s.reference,
                    shown.display()
                );
            }
            None => println!("missing  {:<11} {}", s.agent, s.reference),
        }
    }
    let missing = statuses.iter().filter(|s| s.found.is_none()).count();
    if missing > 0 {
        eprintln!("\n{missing} missing; run `kamili skills install`");
        return Err(Error::Config(format!(
            "{missing} required skill(s) not installed"
        )));
    }
    Ok(())
}

pub fn install(config: &Config, root: &Path) -> Result<(), Error> {
    let statuses = check_skills(config, root)?;
    let mut missing: Vec<&SkillStatus> = Vec::new();
    for s in statuses.iter().filter(|s| s.found.is_none()) {
        if !missing
            .iter()
            .any(|m| m.name == s.name && m.source == s.source)
        {
            missing.push(s);
        }
    }
    if missing.is_empty() {
        println!("all required skills are installed");
        return Ok(());
    }

    let mut no_source = Vec::new();
    for s in missing {
        let Some(source) = &s.source else {
            no_source.push(s.name.clone());
            continue;
        };
        eprintln!("installing {} from {source}…", s.name);
        let status = Command::new(npx())
            .args(["skills", "add", source, "--skill", &s.name, "-y"])
            .current_dir(root)
            .status()
            .map_err(|e| {
                Error::Config(format!(
                    "cannot run `npx` ({e}); install Node.js, or run `{}` yourself",
                    s.install_command().unwrap_or_default()
                ))
            })?;
        if !status.success() {
            return Err(Error::Config(format!(
                "`npx skills add {source} --skill {}` failed",
                s.name
            )));
        }
    }
    for name in &no_source {
        eprintln!(
            "cannot install `{name}`: no source given. Write it as `owner/repo@{name}` in \
             .kamili.toml, or find it with `npx skills find {name}`"
        );
    }

    // The skills CLI decides where files go; confirm Kamili can now see them.
    let still: Vec<String> = check_skills(config, root)?
        .into_iter()
        .filter(|s| s.found.is_none())
        .map(|s| s.reference)
        .collect();
    if still.is_empty() {
        println!("all required skills are installed");
        Ok(())
    } else {
        Err(Error::Config(format!(
            "still missing: {}. If they were installed elsewhere, add that directory to \
             [skills] dirs in .kamili.toml",
            still.join(", ")
        )))
    }
}

fn npx() -> &'static str {
    if cfg!(windows) { "npx.cmd" } else { "npx" }
}
