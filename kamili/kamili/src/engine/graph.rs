//! `kamili graph`: explore the code graph the agents use.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

use crate::config::Config;
use crate::index::{self, CodeGraph, Scope};

use super::tools::{load_graph_cache, save_graph_cache};

use super::Error;

/// What to show from the code graph.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum GraphQuery {
    /// The most important files (towards `focus`, if given) and their key symbols.
    Overview { focus: Option<String>, limit: usize },
    /// The repo map an agent would get for this focus.
    Map { focus: Option<String>, bytes: usize },
    /// Where a name is declared and used.
    Symbol { name: String },
    /// What a file uses and what uses it.
    File { path: String },
    /// Files that use what these files define.
    Impact { paths: Vec<String> },
    /// Graphviz DOT of the top files and their links.
    Dot { focus: Option<String>, limit: usize },
}

/// Builds the code graph for `project_root` and answers `query` as text.
pub fn explore_graph(
    config: &Config,
    project_root: &Path,
    query: &GraphQuery,
) -> Result<String, Error> {
    let c = &config.context;
    let scope =
        Scope::new(&c.include, &c.exclude).map_err(|e| Error::Config(format!("[context] {e}")))?;
    let started = Instant::now();
    let mut cache = load_graph_cache(project_root);
    let graph = CodeGraph::build_cached(project_root, &scope, &mut cache);
    save_graph_cache(project_root, &cache);
    let header = format!(
        "code graph: {} source files, {} symbols, {} file links (built in {} ms, {} file(s) parsed, the rest cached)\n",
        graph.file_count(),
        graph.symbol_count(),
        graph.edge_count(),
        started.elapsed().as_millis(),
        cache.last_parsed()
    );
    let focus_of = |text: &Option<String>| {
        let text = text.as_deref().unwrap_or_default();
        graph.focus(text, &[])
    };

    Ok(match query {
        GraphQuery::Overview { focus, limit } => {
            let f = focus_of(focus);
            let ranking = graph.rank(&f);
            let mut s = header;
            if focus.is_some() {
                let _ = writeln!(
                    s,
                    "focus: {}",
                    if f.is_empty() {
                        "matched no files or names; showing the global ranking".to_string()
                    } else {
                        format!(
                            "{} file(s), {} name(s) matched",
                            f.file_count(),
                            f.name_count()
                        )
                    }
                );
            }
            let top = graph.top_files(&ranking, *limit);
            let max = top.first().map_or(1.0, |(_, r)| *r).max(f64::MIN_POSITIVE);
            let _ = writeln!(s, "\n  RANK  FILE  (key symbols)");
            for (file, rank) in top {
                let _ = writeln!(
                    s,
                    "  {:.3}  {}  ({})",
                    rank / max,
                    graph.path(file),
                    graph.top_symbols(&ranking, file, 5).join(", ")
                );
            }
            s
        }
        GraphQuery::Map { focus, bytes } => {
            let ranking = graph.rank(&focus_of(focus));
            format!(
                "{header}\n{}",
                index::repo_map(project_root, &scope, *bytes, Some((&graph, &ranking)))
            )
        }
        GraphQuery::Symbol { name } => format!(
            "{header}\ndefined:\n{}\n\nused:\n{}\n",
            graph.definitions(name),
            graph.references(project_root, name)
        ),
        GraphQuery::File { path } => {
            let related = graph.related(path).ok_or_else(|| {
                Error::Config(format!(
                    "`{path}` is not a source file in the graph (unsupported language, ignored, or wrong path)"
                ))
            })?;
            format!("{header}\n{related}")
        }
        GraphQuery::Impact { paths } => {
            let hits = graph.impact(paths);
            let mut s = format!(
                "{header}\nfiles using code defined in {}:\n",
                paths.join(", ")
            );
            if hits.is_empty() {
                s.push_str("  (none found)\n");
            }
            for (path, via) in hits {
                let _ = writeln!(s, "  {path}  (via {})", via.join(", "));
            }
            s
        }
        GraphQuery::Dot { focus, limit } => {
            let ranking = graph.rank(&focus_of(focus));
            graph.to_dot(&ranking, *limit)
        }
    })
}
