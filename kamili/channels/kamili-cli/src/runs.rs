//! `kamili runs`: list, show and clean build runs.

use std::path::Path;

use kamili::{Error, clean_runs, discard_run, list_runs};

pub fn list(root: &Path) -> Result<(), Error> {
    let runs = list_runs(root);
    if runs.is_empty() {
        println!("no runs yet");
        return Ok(());
    }
    println!("{:<28} {:<7} {:<44} TASK", "RUN", "TOKENS", "STATE");
    for r in &runs {
        let state: String = r.state.chars().take(44).collect();
        let task: String = r.task.chars().take(50).collect();
        println!("{:<28} {:<7} {:<44} {task}", r.id, tokens(r.tokens), state);
    }
    if let Some(r) = runs.iter().find(|r| r.resumable) {
        eprintln!(
            "\nresume the latest unfinished run with `kamili build --resume` ({})",
            r.id
        );
    }
    Ok(())
}

pub fn show(root: &Path, id: &str) -> Result<(), Error> {
    let runs = list_runs(root);
    let run = if id == "last" {
        runs.first()
    } else {
        runs.iter().find(|r| r.id == id)
    }
    .ok_or_else(|| Error::Config(format!("no run `{id}`; see `kamili runs`")))?;
    let text = std::fs::read_to_string(&run.report)
        .map_err(|e| Error::Workspace(format!("{}: {e}", run.report.display())))?;
    println!("{text}");
    eprintln!("({})", run.report.display());
    Ok(())
}

pub fn clean(root: &Path, all: bool) -> Result<(), Error> {
    let n = clean_runs(root, all)?;
    let what = if all {
        "finished run(s) deleted"
    } else {
        "working copy(ies) removed"
    };
    println!("{n} {what}");
    Ok(())
}

pub fn discard(root: &Path, id: &str) -> Result<(), Error> {
    discard_run(root, id)?;
    println!("discarded {id}");
    Ok(())
}

fn tokens(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}
