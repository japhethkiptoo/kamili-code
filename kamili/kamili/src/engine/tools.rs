//! Runs agents' tool calls. Read tools look at a root (the real project for the
//! Architect and Planner, the working copy for the Worker). Write tools exist
//! only with a working copy, and never touch specs or protected directories.

use std::path::{Path, PathBuf};

use crate::agents::{ToolHost, ToolRequest};
use crate::index::{self, CodeGraph, GraphCache, GrepQuery, Scope};

use super::workspace::WorkingCopy;
use super::{AgentName, Channel, Event};

/// Where the code-graph cache lives, relative to the project root.
const GRAPH_CACHE: &str = ".kamili/cache/graph.json";

/// The project's code-graph cache, or an empty one.
pub(crate) fn load_graph_cache(project_root: &Path) -> GraphCache {
    std::fs::read_to_string(project_root.join(GRAPH_CACHE))
        .map(|t| GraphCache::from_json(&t))
        .unwrap_or_default()
}

/// Saves the cache (best effort: a failed save only costs time next run).
/// Only for caches last built over the real project, not a working copy.
pub(crate) fn save_graph_cache(project_root: &Path, cache: &GraphCache) {
    let path = project_root.join(GRAPH_CACHE);
    let Some(dir) = path.parent() else { return };
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::write(&tmp, cache.to_json()))
        .and_then(|()| std::fs::rename(&tmp, &path));
}

/// Builds the code graph for `root` (reusing `cache`), ranks it towards
/// `focus_text` and `focus_paths`, and renders the repo map. Returns the
/// graph too, so the agent's tools can reuse it.
pub(crate) fn focused_map(
    root: &Path,
    scope: &Scope,
    budget: usize,
    focus_text: &str,
    focus_paths: &[String],
    cache: &mut GraphCache,
) -> (String, CodeGraph) {
    let graph = CodeGraph::build_cached(root, scope, cache);
    let ranking = graph.rank(&graph.focus(focus_text, focus_paths));
    let map = index::repo_map(root, scope, budget, Some((&graph, &ranking)));
    (map, graph)
}

pub(crate) struct ProjectTools<'a> {
    root: PathBuf,
    scope: &'a Scope,
    copy: Option<&'a mut WorkingCopy>,
    /// Relative to the root; writes under it are refused.
    specs_dir: &'a Path,
    ch: &'a dyn Channel,
    agent: AgentName,
    /// Built on first use (or handed in), dropped when the Worker edits.
    graph: Option<CodeGraph>,
    /// Lets a rebuild after an edit re-parse only the edited files.
    cache: GraphCache,
}

impl<'a> ProjectTools<'a> {
    /// Reuses a graph already built for the repo map, and its cache.
    pub fn with_graph(mut self, graph: CodeGraph, cache: GraphCache) -> Self {
        self.graph = Some(graph);
        self.cache = cache;
        self
    }

    fn graph(&mut self) -> &CodeGraph {
        if self.graph.is_none() {
            self.graph = Some(CodeGraph::build_cached(
                &self.root,
                self.scope,
                &mut self.cache,
            ));
        }
        self.graph.as_ref().expect("graph just built")
    }

    /// Read-only tools over `root`.
    pub fn read_only(
        root: &Path,
        scope: &'a Scope,
        specs_dir: &'a Path,
        ch: &'a dyn Channel,
        agent: AgentName,
    ) -> Self {
        Self {
            root: root.to_path_buf(),
            scope,
            copy: None,
            specs_dir,
            ch,
            agent,
            graph: None,
            cache: GraphCache::default(),
        }
    }

    /// Read and write tools over a working copy.
    pub fn working_copy(
        copy: &'a mut WorkingCopy,
        scope: &'a Scope,
        specs_dir: &'a Path,
        ch: &'a dyn Channel,
        agent: AgentName,
    ) -> Self {
        Self {
            root: copy.path().to_path_buf(),
            scope,
            copy: Some(copy),
            specs_dir,
            ch,
            agent,
            graph: None,
            cache: GraphCache::default(),
        }
    }

    fn write(&mut self, path: &str, content: Option<&str>) -> Result<(), String> {
        let rel = index::checked(path).map_err(|e| e.to_string())?;
        if rel.as_os_str().is_empty() {
            return Err("give a file path".into());
        }
        if rel.starts_with(self.specs_dir) {
            return Err(format!(
                "`{path}` is a spec file; specs are edited by other agents, not the Worker"
            ));
        }
        let copy = self
            .copy
            .as_deref_mut()
            .ok_or("this agent has read-only access")?;
        copy.apply(path, content).map_err(|e| e.to_string())?;
        self.graph = None; // stale now
        Ok(())
    }

    fn replace(&mut self, path: &str, old: &str, new: &str, all: bool) -> Result<String, String> {
        if old.is_empty() {
            return Err("old_text is empty; use write_file to create a file".into());
        }
        let rel = index::checked(path).map_err(|e| e.to_string())?;
        let full = self.root.join(&rel);
        if !full.is_file() {
            return Err(format!(
                "`{path}` does not exist; use write_file to create it"
            ));
        }
        let text = index::read_text(&full).map_err(|e| e.to_string())?;
        let count = text.matches(old).count();
        let updated = match (count, all) {
            (0, _) => {
                return Err(format!(
                    "old_text not found in {path}; read the lines again and copy them exactly, including indentation"
                ));
            }
            (1, _) | (_, true) => text.replacen(old, new, if all { count } else { 1 }),
            (n, false) => {
                return Err(format!(
                    "old_text occurs {n} times in {path}; include more surrounding lines to make it unique, or set replace_all"
                ));
            }
        };
        self.write(path, Some(&updated))?;
        Ok(format!(
            "replaced {} occurrence(s) in {path}",
            if all { count } else { 1 }
        ))
    }
}

impl ToolHost for ProjectTools<'_> {
    fn run(&mut self, request: &ToolRequest) -> Result<String, String> {
        self.ch.event(Event::ToolUse {
            agent: self.agent,
            description: request.describe(),
        });
        let err = |e: index::IndexError| e.to_string();
        match request {
            ToolRequest::ListDir { path } => {
                index::list_dir(&self.root, path, self.scope).map_err(err)
            }
            ToolRequest::FindFiles { pattern } => {
                index::glob(&self.root, pattern, self.scope).map_err(err)
            }
            ToolRequest::Grep {
                pattern,
                path,
                glob,
                case_insensitive,
            } => index::grep(
                &self.root,
                &GrepQuery {
                    pattern: pattern.clone(),
                    path: path.clone(),
                    glob: glob.clone(),
                    case_insensitive: *case_insensitive,
                },
                self.scope,
            )
            .map_err(err),
            ToolRequest::ReadFile {
                path,
                start_line,
                end_line,
            } => index::read(&self.root, path, *start_line, *end_line).map_err(err),
            ToolRequest::Outline { path } => index::outline_file(&self.root, path).map_err(err),
            ToolRequest::FindSymbol { name } => Ok(self.graph().definitions(name.trim())),
            ToolRequest::References { name } => {
                let root = self.root.clone();
                Ok(self.graph().references(&root, name.trim()))
            }
            ToolRequest::RelatedFiles { path } => self.graph().related(path).ok_or_else(|| {
                format!(
                    "`{path}` is not a source file in the graph; check the path with find_files"
                )
            }),
            ToolRequest::WriteFile { path, content } => {
                self.write(path, Some(content))?;
                Ok(format!("wrote {path} ({} lines)", content.lines().count()))
            }
            ToolRequest::ReplaceInFile {
                path,
                old_text,
                new_text,
                replace_all,
            } => self.replace(path, old_text, new_text, *replace_all),
            ToolRequest::DeleteFile { path } => {
                let rel = index::checked(path).map_err(err)?;
                if !self.root.join(&rel).is_file() {
                    return Err(format!("`{path}` does not exist"));
                }
                self.write(path, None)?;
                Ok(format!("deleted {path}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{ApprovalRequest, Decision, Question};

    struct Quiet;
    #[async_trait::async_trait]
    impl Channel for Quiet {
        fn event(&self, _: Event) {}
        async fn ask(&self, _: &Question) -> Option<String> {
            None
        }
        async fn approve(&self, _: &ApprovalRequest) -> Decision {
            Decision::Reject
        }
    }

    #[test]
    fn edits_go_to_the_copy_and_respect_rules() {
        let real = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(real.path().join("docs/features")).unwrap();
        std::fs::write(
            real.path().join("a.rs"),
            "fn a() {}\nfn b() {}\nfn b() {}\n",
        )
        .unwrap();
        let work = tempfile::tempdir().unwrap();
        let mut copy = WorkingCopy::create(real.path(), &work.path().join("w")).unwrap();
        let scope = Scope::default();
        let specs = Path::new("docs/features");
        let mut t = ProjectTools::working_copy(&mut copy, &scope, specs, &Quiet, AgentName::Worker);

        let r = |old: &str, new: &str, all| ToolRequest::ReplaceInFile {
            path: "a.rs".into(),
            old_text: old.into(),
            new_text: new.into(),
            replace_all: all,
        };
        assert!(t.run(&r("fn a() {}", "fn a() { 1 }", false)).is_ok());
        assert!(
            t.run(&r("fn b() {}", "x", false))
                .unwrap_err()
                .contains("2 times")
        );
        assert!(
            t.run(&r("fn c", "x", false))
                .unwrap_err()
                .contains("not found")
        );
        assert!(t.run(&r("fn b() {}", "fn bb() {}", true)).is_ok());
        let spec = ToolRequest::WriteFile {
            path: "docs/features/x.md".into(),
            content: "x".into(),
        };
        assert!(t.run(&spec).unwrap_err().contains("spec file"));
        let escape = ToolRequest::WriteFile {
            path: "../evil".into(),
            content: "x".into(),
        };
        assert!(t.run(&escape).is_err());
        // Graph tools see the edit just made.
        let find = ToolRequest::FindSymbol { name: "bb".into() };
        assert!(
            t.run(&find).unwrap().contains("no definition"),
            "too short to index"
        );
        drop(t);

        assert_eq!(copy.changed_paths(), ["a.rs"]);
        assert_eq!(
            std::fs::read_to_string(copy.path().join("a.rs")).unwrap(),
            "fn a() { 1 }\nfn bb() {}\nfn bb() {}\n"
        );
        // The real tree is untouched until write-back.
        assert!(
            std::fs::read_to_string(real.path().join("a.rs"))
                .unwrap()
                .starts_with("fn a() {}")
        );
    }

    #[test]
    fn read_only_host_refuses_writes() {
        let real = tempfile::tempdir().unwrap();
        let scope = Scope::default();
        let mut t = ProjectTools::read_only(
            real.path(),
            &scope,
            Path::new("docs"),
            &Quiet,
            AgentName::Planner,
        );
        let w = ToolRequest::WriteFile {
            path: "a".into(),
            content: "x".into(),
        };
        assert!(t.run(&w).unwrap_err().contains("read-only"));
    }
}
