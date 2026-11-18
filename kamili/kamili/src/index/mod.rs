//! Codebase scanning: an ignore-aware file walk, glob, grep, ranged reads,
//! symbol outlines, and a compact repository map. Leaf module.
//!
//! Agents never get the whole tree. They start from a capped repo map and look
//! things up with these functions (exposed to them as tools by the engine).
//! Every result is capped so one call can't flood a prompt.

mod graph;
mod outline;

pub(crate) use graph::{CodeGraph, GraphCache, Ranking};
pub(crate) use outline::outline;

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

/// Directories never scanned, whatever the ignore files say.
const ALWAYS_SKIP: &[&str] = &[".git", ".kamili", "target", "node_modules"];
/// Extra ignore file honoured next to `.gitignore`.
const KAMILI_IGNORE: &str = ".kamiliignore";
/// Files larger than this are listed but never searched or read whole.
const MAX_SCAN_BYTES: u64 = 1_000_000;
const MAX_READ_LINES: usize = 400;
const MAX_READ_BYTES: usize = 40_000;
const MAX_LINE_CHARS: usize = 400;
pub(crate) const MAX_MATCHES: usize = 100;
const MAX_LISTED: usize = 500;

#[derive(Debug, thiserror::Error)]
pub(crate) enum IndexError {
    #[error("invalid path `{0}`: use a relative path inside the project")]
    BadPath(String),
    #[error("invalid pattern `{pattern}`: {reason}")]
    BadPattern { pattern: String, reason: String },
    #[error("`{0}` not found")]
    NotFound(String),
    #[error("`{0}` is a binary or very large file")]
    NotText(String),
    #[error("{0}")]
    Io(String),
}

impl From<std::io::Error> for IndexError {
    fn from(e: std::io::Error) -> Self {
        IndexError::Io(e.to_string())
    }
}

/// Which files are in scope (`[context] include/exclude`, as globs).
#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    include: Option<GlobSet>,
    exclude: Option<GlobSet>,
}

impl Scope {
    pub fn new(include: &[String], exclude: &[String]) -> Result<Self, IndexError> {
        Ok(Self {
            include: glob_set(include)?,
            exclude: glob_set(exclude)?,
        })
    }

    fn admits(&self, rel: &str) -> bool {
        self.include.as_ref().is_none_or(|g| g.is_match(rel))
            && !self.exclude.as_ref().is_some_and(|g| g.is_match(rel))
    }
}

fn glob_set(patterns: &[String]) -> Result<Option<GlobSet>, IndexError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(compile_glob(p)?);
    }
    b.build().map(Some).map_err(|e| IndexError::BadPattern {
        pattern: patterns.join(", "),
        reason: e.to_string(),
    })
}

fn compile_glob(p: &str) -> Result<Glob, IndexError> {
    globset::GlobBuilder::new(p)
        .literal_separator(true)
        .build()
        .map_err(|e| IndexError::BadPattern {
            pattern: p.to_string(),
            reason: e.to_string(),
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileEntry {
    /// Relative, `/`-separated.
    pub path: String,
    pub size: u64,
    /// Modification time in nanoseconds since the epoch (0 if unknown).
    pub modified: u64,
}

/// Stable content fingerprint: length and 64-bit FNV-1a.
pub(crate) fn fingerprint(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{}:{h:016x}", bytes.len())
}

/// A relative path with only normal components (`.` allowed for the root).
pub(crate) fn checked(path: &str) -> Result<PathBuf, IndexError> {
    let trimmed = path.trim().trim_start_matches("./");
    if trimmed.is_empty() || trimmed == "." {
        return Ok(PathBuf::new());
    }
    let p = Path::new(trimmed);
    let ok = p.components().all(|c| match c {
        Component::Normal(n) => !ALWAYS_SKIP.contains(&n.to_string_lossy().as_ref()),
        _ => false,
    });
    if ok {
        Ok(p.to_path_buf())
    } else {
        Err(IndexError::BadPath(path.to_string()))
    }
}

/// Walker over `root` honouring `.gitignore` (no git repository needed),
/// `.kamiliignore`, and the always-skipped directories. Ignore files above
/// `root` are not read, so a parent project's rules never leak in.
fn walker(root: &Path, max_depth: Option<usize>) -> ignore::Walk {
    let mut b = ignore::WalkBuilder::new(root);
    b.hidden(false)
        .parents(false)
        .require_git(false)
        .git_global(false)
        .follow_links(false)
        .max_depth(max_depth)
        .add_custom_ignore_filename(KAMILI_IGNORE)
        .filter_entry(|e| !ALWAYS_SKIP.contains(&e.file_name().to_string_lossy().as_ref()))
        .sort_by_file_name(|a, b| a.cmp(b));
    b.build()
}

fn rel_of(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    (!s.is_empty()).then_some(s)
}

/// Every in-scope file under `root`, sorted.
pub(crate) fn files(root: &Path, scope: &Scope) -> Vec<FileEntry> {
    walker(root, None)
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|e| {
            let path = rel_of(root, e.path())?;
            scope.admits(&path).then(|| {
                let meta = e.metadata().ok();
                FileEntry {
                    size: meta.as_ref().map_or(0, |m| m.len()),
                    modified: meta
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_nanos() as u64),
                    path,
                }
            })
        })
        .collect()
}

/// One directory level: subdirectories end with `/`, files show their size.
pub(crate) fn list_dir(root: &Path, dir: &str, scope: &Scope) -> Result<String, IndexError> {
    let rel = checked(dir)?;
    let target = root.join(&rel);
    if !target.is_dir() {
        return Err(IndexError::NotFound(dir.to_string()));
    }
    let depth = rel.components().count() + 1;
    let mut lines = Vec::new();
    let mut total = 0;
    // Walk from the root (so ignore files apply), pruning everything that is
    // not on the way to `dir`.
    for e in walker(root, Some(depth)).filter_map(Result::ok) {
        let Some(path) = rel_of(root, e.path()) else {
            continue;
        };
        let p = Path::new(&path);
        if p.parent() != Some(rel.as_path()) {
            continue;
        }
        let is_dir = e.file_type().is_some_and(|t| t.is_dir());
        if !is_dir && !scope.admits(&path) {
            continue;
        }
        total += 1;
        if lines.len() < MAX_LISTED {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            lines.push(if is_dir {
                format!("{name}/")
            } else {
                format!(
                    "{name}  ({})",
                    human_size(e.metadata().map(|m| m.len()).unwrap_or(0))
                )
            });
        }
    }
    if total > lines.len() {
        lines.push(format!("… {} more entries", total - lines.len()));
    }
    if lines.is_empty() {
        return Ok("(empty)".into());
    }
    Ok(lines.join("\n"))
}

/// Paths matching a glob such as `src/**/*.rs`.
pub(crate) fn glob(root: &Path, pattern: &str, scope: &Scope) -> Result<String, IndexError> {
    let matcher = compile_glob(pattern)?.compile_matcher();
    let hits: Vec<String> = files(root, scope)
        .into_iter()
        .filter(|f| matcher.is_match(&f.path))
        .map(|f| f.path)
        .collect();
    Ok(capped_list(hits, MAX_LISTED, "files"))
}

#[derive(Debug, Clone, Default)]
pub(crate) struct GrepQuery {
    pub pattern: String,
    /// Directory or file to search under (default: everything).
    pub path: Option<String>,
    /// Only files matching this glob.
    pub glob: Option<String>,
    pub case_insensitive: bool,
}

/// `path:line: text` for each match of a regex, capped at [`MAX_MATCHES`].
pub(crate) fn grep(root: &Path, q: &GrepQuery, scope: &Scope) -> Result<String, IndexError> {
    let re = regex::RegexBuilder::new(&q.pattern)
        .case_insensitive(q.case_insensitive)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| IndexError::BadPattern {
            pattern: q.pattern.clone(),
            reason: e.to_string(),
        })?;
    // Whole-file pre-check: the same pattern with `^`/`$` matching at line
    // boundaries, so files without a match are skipped without splitting lines.
    let prefilter = regex::RegexBuilder::new(&q.pattern)
        .case_insensitive(q.case_insensitive)
        .multi_line(true)
        .size_limit(1 << 20)
        .build()
        .map_err(|e| IndexError::BadPattern {
            pattern: q.pattern.clone(),
            reason: e.to_string(),
        })?;
    let under = q.path.as_deref().map(checked).transpose()?;
    let only = q
        .glob
        .as_deref()
        .map(|g| compile_glob(g).map(|g| g.compile_matcher()))
        .transpose()?;

    let candidates: Vec<FileEntry> = files(root, scope)
        .into_iter()
        .filter(|f| {
            !under
                .as_ref()
                .is_some_and(|u| !u.as_os_str().is_empty() && !Path::new(&f.path).starts_with(u))
        })
        .filter(|f| only.as_ref().is_none_or(|m| m.is_match(&f.path)) && f.size <= MAX_SCAN_BYTES)
        .collect();

    // Search in parallel over contiguous chunks, so results stay in path order.
    let threads = std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .min(16);
    let chunk = candidates.len().div_ceil(threads).max(64);
    let per_chunk: Vec<(Vec<String>, usize)> = std::thread::scope(|sc| {
        let handles: Vec<_> = candidates
            .chunks(chunk)
            .map(|files| {
                let (re, prefilter) = (&re, &prefilter);
                sc.spawn(move || {
                    let mut hits = Vec::new();
                    let mut count = 0usize;
                    for f in files {
                        let Ok(text) = read_text(&root.join(&f.path)) else {
                            continue;
                        };
                        if !prefilter.is_match(&text) {
                            continue;
                        }
                        for (i, line) in text.lines().enumerate() {
                            if re.is_match(line) {
                                count += 1;
                                if hits.len() < MAX_MATCHES {
                                    hits.push(format!(
                                        "{}:{}: {}",
                                        f.path,
                                        i + 1,
                                        clip(line.trim_end())
                                    ));
                                }
                            }
                        }
                    }
                    (hits, count)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_default())
            .collect()
    });
    let total: usize = per_chunk.iter().map(|(_, n)| n).sum();
    let mut out: Vec<String> = per_chunk
        .into_iter()
        .flat_map(|(hits, _)| hits)
        .take(MAX_MATCHES)
        .collect();
    if total == 0 {
        return Ok("no matches".into());
    }
    if total > out.len() {
        out.push(format!(
            "… {} more matches; narrow the pattern, path or glob",
            total - out.len()
        ));
    }
    Ok(out.join("\n"))
}

/// Lines `start..=end` (1-based) with line numbers, capped.
pub(crate) fn read(
    root: &Path,
    path: &str,
    start: Option<usize>,
    end: Option<usize>,
) -> Result<String, IndexError> {
    let rel = checked(path)?;
    let full = root.join(&rel);
    if !full.is_file() {
        return Err(IndexError::NotFound(path.to_string()));
    }
    let text = read_text(&full).map_err(|_| IndexError::NotText(path.to_string()))?;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let first = start.unwrap_or(1).max(1);
    if first > total.max(1) {
        return Ok(format!("(file has {total} lines)"));
    }
    let last = end
        .unwrap_or(total)
        .min(total)
        .min(first + MAX_READ_LINES - 1);
    let mut out = String::new();
    let mut shown = first - 1;
    for (i, line) in lines[first - 1..last].iter().enumerate() {
        let row = format!("{:>5}  {}\n", first + i, clip(line));
        if out.len() + row.len() > MAX_READ_BYTES {
            break;
        }
        out.push_str(&row);
        shown = first + i;
    }
    if shown < total {
        out.push_str(&format!(
            "(lines {first}-{shown} of {total}; read more with start_line = {})\n",
            shown + 1
        ));
    }
    Ok(out)
}

/// Text of a file, if it is small enough and not binary.
pub(crate) fn read_text(path: &Path) -> Result<String, IndexError> {
    let meta = fs::metadata(path)?;
    if meta.len() > MAX_SCAN_BYTES * 4 {
        return Err(IndexError::NotText(path.display().to_string()));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    fs::File::open(path)?.read_to_end(&mut bytes)?;
    if bytes.iter().take(8_000).any(|b| *b == 0) {
        return Err(IndexError::NotText(path.display().to_string()));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Symbols in a file (functions, types, …) with line numbers.
pub(crate) fn outline_file(root: &Path, path: &str) -> Result<String, IndexError> {
    let rel = checked(path)?;
    let full = root.join(&rel);
    if !full.is_file() {
        return Err(IndexError::NotFound(path.to_string()));
    }
    let text = read_text(&full).map_err(|_| IndexError::NotText(path.to_string()))?;
    let symbols = outline(path, &text);
    if symbols.is_empty() {
        return Ok("no symbols found (unsupported language, or none at top level)".into());
    }
    Ok(symbols
        .iter()
        .take(MAX_LISTED)
        .map(|s| format!("{:>5}  {}", s.line, s.signature))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// A compact overview of the project within `budget` bytes: root manifests
/// and README excerpt; then, with a code graph, the most relevant files and
/// declarations for the current focus; then the directory tree with file
/// counts, listing files down to the deepest level that fits.
pub(crate) fn repo_map(
    root: &Path,
    scope: &Scope,
    budget: usize,
    ranked: Option<(&CodeGraph, &Ranking)>,
) -> String {
    let all = files(root, scope);
    let mut out = format!("{} files in scope.\n", all.len());

    // Root-level orientation files, briefly.
    for name in [
        "README.md",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
        "build.gradle.kts",
        "Gemfile",
        "composer.json",
    ] {
        if out.len() > budget / 4 {
            break;
        }
        if all.iter().any(|f| f.path == name)
            && let Ok(text) = read_text(&root.join(name))
        {
            let excerpt: String = text.chars().take(1_200).collect();
            let more = if excerpt.len() < text.len() {
                "\n…"
            } else {
                ""
            };
            out.push_str(&format!(
                "\n<{name}>\n{}{more}\n</{name}>\n",
                excerpt.trim_end()
            ));
        }
    }

    if let Some((graph, ranking)) = ranked {
        let room = budget.saturating_sub(out.len()) * 3 / 5;
        let map = graph.ranked_map(ranking, room);
        if !map.is_empty() {
            out.push_str(
                "\n<key_symbols>\n(files and declarations most relevant to this task, most relevant first; line numbers on the left)\n",
            );
            out.push_str(&map);
            out.push_str("</key_symbols>\n");
        }
    }

    let remaining = budget.saturating_sub(out.len());
    // Deepest file-listing depth whose tree fits the remaining budget.
    let tree = (0..=8)
        .rev()
        .map(|depth| tree(&all, depth))
        .find(|t| t.len() <= remaining)
        .unwrap_or_else(|| {
            let t = tree(&all, 0);
            let cut: String = t.chars().take(remaining.saturating_sub(40)).collect();
            format!("{cut}\n… (tree truncated)")
        });
    out.push_str("\n<tree>\n");
    out.push_str(&tree);
    out.push_str("</tree>\n");
    out
}

/// Directory tree. Directories show file counts; files are listed only in
/// directories at depth < `file_depth`.
fn tree(all: &[FileEntry], file_depth: usize) -> String {
    let mut dirs: BTreeMap<String, (usize, Vec<&str>)> = BTreeMap::new();
    for f in all {
        let p = Path::new(&f.path);
        let parent = p
            .parent()
            .map(|d| d.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        // Count the file in every ancestor directory.
        let mut acc = String::new();
        dirs.entry(String::new()).or_default().0 += 1;
        for part in parent.split('/').filter(|s| !s.is_empty()) {
            acc = if acc.is_empty() {
                part.to_string()
            } else {
                format!("{acc}/{part}")
            };
            dirs.entry(acc.clone()).or_default().0 += 1;
        }
        dirs.entry(parent).or_default().1.push(&f.path);
    }
    let mut out = String::new();
    for (dir, (count, own)) in &dirs {
        let depth = if dir.is_empty() {
            0
        } else {
            dir.split('/').count()
        };
        if depth > file_depth + 1 {
            continue;
        }
        let indent = "  ".repeat(depth);
        if !dir.is_empty() {
            let name = dir.rsplit('/').next().unwrap_or(dir);
            out.push_str(&format!(
                "{}{name}/ ({count} files)\n",
                "  ".repeat(depth - 1)
            ));
        }
        if depth < file_depth || (depth == 0 && file_depth == 0) {
            for f in own {
                let name = f.rsplit('/').next().unwrap_or(f);
                out.push_str(&format!("{indent}{name}\n"));
            }
        }
    }
    out
}

fn capped_list(items: Vec<String>, max: usize, what: &str) -> String {
    if items.is_empty() {
        return format!("no {what}");
    }
    let total = items.len();
    let mut shown: Vec<String> = items.into_iter().take(max).collect();
    if total > max {
        shown.push(format!("… {} more {what}", total - max));
    }
    shown.join("\n")
}

fn clip(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(MAX_LINE_CHARS).collect();
    format!("{cut}…")
}

fn human_size(n: u64) -> String {
    match n {
        0..=9_999 => format!("{n} B"),
        10_000..=9_999_999 => format!("{} KB", n / 1_000),
        _ => format!("{} MB", n / 1_000_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let w = |p: &str, s: &str| {
            let path = d.path().join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, s).unwrap();
        };
        w(".gitignore", "dist/\n*.log\n");
        w(".kamiliignore", "vendor/\n");
        w("README.md", "# Demo\nA demo project.\n");
        w("src/main.rs", "fn main() {\n    greet(\"x\");\n}\n");
        w(
            "src/lib/greet.rs",
            "pub fn greet(name: &str) -> String {\n    format!(\"hi {name}\")\n}\n",
        );
        w("dist/bundle.js", "ignored");
        w("vendor/x.rs", "fn greet() {}");
        w("debug.log", "greet");
        w("node_modules/a/index.js", "greet");
        w(".github/ci.yml", "on: push");
        d
    }

    #[test]
    fn walk_honours_ignore_files_without_git() {
        let d = project();
        let paths: Vec<String> = files(d.path(), &Scope::default())
            .into_iter()
            .map(|f| f.path)
            .collect();
        assert_eq!(
            paths,
            [
                ".github/ci.yml",
                ".gitignore",
                ".kamiliignore",
                "README.md",
                "src/lib/greet.rs",
                "src/main.rs"
            ]
        );
    }

    #[test]
    fn scope_include_exclude() {
        let d = project();
        let scope = Scope::new(&["src/**".into()], &["src/lib/**".into()]).unwrap();
        let paths: Vec<String> = files(d.path(), &scope)
            .into_iter()
            .map(|f| f.path)
            .collect();
        assert_eq!(paths, ["src/main.rs"]);
    }

    #[test]
    fn grep_glob_and_list() {
        let d = project();
        let s = Scope::default();
        let q = GrepQuery {
            pattern: r"fn greet".into(),
            ..Default::default()
        };
        assert_eq!(
            grep(d.path(), &q, &s).unwrap(),
            "src/lib/greet.rs:1: pub fn greet(name: &str) -> String {"
        );
        let q = GrepQuery {
            pattern: "GREET".into(),
            path: Some("src".into()),
            glob: Some("**/main.rs".into()),
            case_insensitive: true,
        };
        assert_eq!(
            grep(d.path(), &q, &s).unwrap(),
            "src/main.rs:2:     greet(\"x\");"
        );
        assert!(
            grep(
                d.path(),
                &GrepQuery {
                    pattern: "(".into(),
                    ..Default::default()
                },
                &s
            )
            .is_err()
        );
        // Anchors match per line, not per file.
        let anchored = GrepQuery {
            pattern: r"^\s+greet".into(),
            ..Default::default()
        };
        assert_eq!(
            grep(d.path(), &anchored, &s).unwrap(),
            "src/main.rs:2:     greet(\"x\");"
        );

        assert_eq!(
            glob(d.path(), "src/**/*.rs", &s).unwrap(),
            "src/lib/greet.rs\nsrc/main.rs"
        );
        let listing = list_dir(d.path(), "src", &s).unwrap();
        assert_eq!(listing, "lib/\nmain.rs  (30 B)");
        assert!(list_dir(d.path(), "../", &s).is_err());
        assert!(list_dir(d.path(), "dist", &s).unwrap().contains("empty"));
    }

    #[test]
    fn read_ranges_and_paths() {
        let d = project();
        let all = read(d.path(), "src/main.rs", None, None).unwrap();
        assert!(all.starts_with("    1  fn main() {\n"));
        let mid = read(d.path(), "src/main.rs", Some(2), Some(2)).unwrap();
        assert!(mid.starts_with("    2      greet(\"x\");\n(lines 2-2 of 3"));
        for bad in ["/etc/passwd", "../x", ".git/config", "src/../../x"] {
            assert!(read(d.path(), bad, None, None).is_err(), "{bad}");
        }
        fs::write(d.path().join("bin.dat"), [0u8, 1, 2]).unwrap();
        assert!(matches!(
            read(d.path(), "bin.dat", None, None),
            Err(IndexError::NotText(_))
        ));
    }

    #[test]
    fn repo_map_fits_budget() {
        let d = project();
        let map = repo_map(d.path(), &Scope::default(), 4_000, None);
        assert!(
            map.contains("<README.md>") && map.contains("src/ (2 files)"),
            "{map}"
        );
        assert!(map.contains("greet.rs"));
        let small = repo_map(d.path(), &Scope::default(), 200, None);
        let g = CodeGraph::build(d.path(), &Scope::default());
        let r = g.rank(&g.focus("change greet", &[]));
        let ranked = repo_map(d.path(), &Scope::default(), 4_000, Some((&g, &r)));
        assert!(
            ranked.contains("<key_symbols>")
                && ranked.contains("src/lib/greet.rs\n     1  pub fn greet"),
            "{ranked}"
        );
        assert!(small.len() < 400, "{small}");
    }
}
