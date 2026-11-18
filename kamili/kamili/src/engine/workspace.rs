//! Working copies, write-back, and `.kamili/` records (run reports, transcripts).
//!
//! All build work happens in a temporary copy. Write-back is a single final
//! step that first checks nothing it is about to overwrite changed on disk.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::Error;

/// Directory names never copied into a working copy or listed for agents.
const SKIP: &[&str] = &[".git", ".kamili", "target", "node_modules"];
const STATE_DIR: &str = ".kamili";
const MAX_LISTED_FILES: usize = 2_000;

/// A copy of the project where agents work and tests run. Lives outside the
/// project (so build tools and ignore files of the real tree don't see it) in a
/// per-run directory that survives crashes, so a run can resume. Serializable:
/// the run state records it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkingCopy {
    real_root: PathBuf,
    dir: PathBuf,
    /// Touched paths → fingerprint of their contents at copy time
    /// (`None` = did not exist).
    originals: BTreeMap<String, Option<String>>,
}

impl WorkingCopy {
    /// Copies `real_root` into `dir`, which must not exist yet.
    pub fn create(real_root: &Path, dir: &Path) -> Result<Self, Error> {
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        fs::create_dir_all(dir)?;
        copy_tree(real_root, dir)?;
        Ok(Self {
            real_root: real_root.to_path_buf(),
            dir: dir.to_path_buf(),
            originals: BTreeMap::new(),
        })
    }

    /// Fails if the directory is gone (for example, the temp dir was cleaned).
    pub fn check_exists(&self) -> Result<(), Error> {
        if self.dir.is_dir() {
            Ok(())
        } else {
            Err(Error::Workspace(format!(
                "working copy {} no longer exists; start a new build",
                self.dir.display()
            )))
        }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Applies one change to the copy only. Rejects unsafe paths.
    pub fn apply(&mut self, path: &str, content: Option<&str>) -> Result<(), Error> {
        let rel = safe_relative(path)?;
        let key = rel.to_string_lossy().replace('\\', "/");
        let target = self.dir.join(&rel);
        if !self.originals.contains_key(&key) {
            let before = fs::read(self.real_root.join(&rel)).ok();
            self.originals
                .insert(key, before.as_deref().map(fingerprint));
        }
        match content {
            Some(text) => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, text)?;
            }
            None => match fs::remove_file(&target) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            },
        }
        Ok(())
    }

    /// Touched paths whose contents in the copy differ from the original.
    pub fn changed_paths(&self) -> Vec<String> {
        self.originals
            .iter()
            .filter(|(path, original)| {
                let now = fs::read(self.dir.join(path)).ok();
                now.as_deref().map(fingerprint) != **original
            })
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// Fails if any path we will write changed in the real tree since the copy.
    pub fn check_conflicts(&self) -> Result<(), Error> {
        for path in self.changed_paths() {
            let current = fs::read(self.real_root.join(&path)).ok();
            if current.as_deref().map(fingerprint) != self.originals[&path] {
                return Err(Error::Conflict(format!(
                    "{path} changed on disk during the run"
                )));
            }
        }
        Ok(())
    }

    /// Copies changed paths to the real tree. Call `check_conflicts` first.
    pub fn write_back(&self) -> Result<(), Error> {
        for path in self.changed_paths() {
            let target = self.real_root.join(&path);
            match fs::read(self.dir.join(&path)) {
                Ok(bytes) => {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&target, bytes)?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::remove_file(&target) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                    _ => {}
                },
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Deletes the copy.
    pub fn remove(&self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Where a run's working copy lives: under the system temp dir, keyed by run
/// and project so two projects never share one.
pub(crate) fn work_dir_for(project_root: &Path, run_id: &str) -> PathBuf {
    let project = fingerprint(project_root.to_string_lossy().as_bytes());
    std::env::temp_dir()
        .join("kamili-work")
        .join(format!("{run_id}-{}", &project[project.len() - 8..]))
}

pub(crate) use crate::index::fingerprint;

/// A relative path with only normal components, outside skipped directories.
fn safe_relative(path: &str) -> Result<PathBuf, Error> {
    let p = Path::new(path);
    let ok = !path.is_empty()
        && p.components().all(|c| matches!(c, Component::Normal(_)))
        && p.components()
            .all(|c| !SKIP.contains(&c.as_os_str().to_string_lossy().as_ref()));
    if ok {
        Ok(p.to_path_buf())
    } else {
        Err(Error::Agent(format!("refusing unsafe path `{path}`")))
    }
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        if SKIP.contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        let kind = entry.file_type()?; // does not follow symlinks
        let dest = to.join(&name);
        if kind.is_dir() {
            fs::create_dir_all(&dest)?;
            copy_tree(&entry.path(), &dest)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &dest)?;
        }
        // Symlinks are skipped so a copy can never reach outside the project.
    }
    Ok(())
}

/// Relative file paths under `root`, sorted, skipping `SKIP` dirs. Capped.
pub(crate) fn list_files(root: &Path) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            if SKIP.contains(&entry.file_name().to_string_lossy().as_ref()) {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file()
                && let Ok(rel) = entry.path().strip_prefix(root)
            {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    out.sort();
    out.truncate(MAX_LISTED_FILES);
    Ok(out)
}

/// Writes `value` as pretty JSON to `.kamili/<sub>/<name>.json`.
pub(crate) fn save_record(
    root: &Path,
    sub: &str,
    name: &str,
    value: &impl Serialize,
) -> Result<PathBuf, Error> {
    let dir = root.join(STATE_DIR).join(sub);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.json"));
    let json = serde_json::to_string_pretty(value).map_err(|e| Error::Workspace(e.to_string()))?;
    fs::write(&path, json)?;
    Ok(path)
}

pub(crate) fn read_record<T: serde::de::DeserializeOwned>(
    root: &Path,
    sub: &str,
    name: &str,
) -> Option<T> {
    let text =
        fs::read_to_string(root.join(STATE_DIR).join(sub).join(format!("{name}.json"))).ok()?;
    serde_json::from_str(&text).ok()
}

pub(crate) fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escaping_and_protected_paths() {
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            ".git/config",
            "src/.kamili/x",
            "./a",
        ] {
            assert!(safe_relative(bad).is_err(), "{bad} should be rejected");
        }
        assert!(safe_relative("src/main.rs").is_ok());
    }

    #[test]
    fn copy_apply_conflict_and_write_back() {
        let real = tempfile::tempdir().unwrap();
        fs::write(real.path().join("a.txt"), "one").unwrap();
        fs::create_dir(real.path().join(".git")).unwrap();

        let work = tempfile::tempdir().unwrap();
        let mut wc = WorkingCopy::create(real.path(), &work.path().join("w")).unwrap();
        assert_eq!(list_files(wc.path()).unwrap(), ["a.txt"]);

        wc.apply("a.txt", Some("two")).unwrap();
        wc.apply("new/b.txt", Some("b")).unwrap();
        wc.apply("tmp.txt", Some("x")).unwrap();
        wc.apply("tmp.txt", None).unwrap(); // created then deleted: no change
        assert_eq!(wc.changed_paths(), ["a.txt", "new/b.txt"]);
        assert_eq!(
            fs::read_to_string(real.path().join("a.txt")).unwrap(),
            "one"
        );

        wc.check_conflicts().unwrap();
        wc.write_back().unwrap();
        assert_eq!(
            fs::read_to_string(real.path().join("a.txt")).unwrap(),
            "two"
        );
        assert_eq!(
            fs::read_to_string(real.path().join("new/b.txt")).unwrap(),
            "b"
        );

        // Survives a round trip through the run state.
        let json = serde_json::to_string(&wc).unwrap();
        let wc2: WorkingCopy = serde_json::from_str(&json).unwrap();
        assert_eq!(wc2.changed_paths(), ["a.txt", "new/b.txt"]);

        // A file edited on disk after the copy is a conflict.
        let mut wc = WorkingCopy::create(real.path(), &work.path().join("w2")).unwrap();
        wc.apply("a.txt", Some("three")).unwrap();
        fs::write(real.path().join("a.txt"), "edited by human").unwrap();
        assert!(matches!(wc.check_conflicts(), Err(Error::Conflict(_))));
    }
}
