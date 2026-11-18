//! A codebase graph built from outlines and identifiers, with no parsers.
//!
//! - **Nodes** are source files, and the symbols each one defines.
//! - **Edges** go from file A to file B when A mentions a name B defines.
//!   Distinctive names weigh more; names defined in many files weigh less;
//!   names the current task mentions weigh much more.
//! - **Ranking** is PageRank over those edges, optionally personalised towards
//!   a focus (files and names from the task and spec). Files that a lot of
//!   relevant code depends on rank highest; a file's rank is shared among its
//!   symbols by how much each is referenced.
//!
//! Name matching is approximate: two unrelated `parse` functions look alike.
//! That's acceptable for ranking and navigation; agents confirm by reading.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use super::outline::{Symbol, outline, supported};
use super::{FileEntry, MAX_MATCHES, MAX_SCAN_BYTES, Scope, clip, files, fingerprint, read_text};

use serde::{Deserialize, Serialize};

/// Names defined in more files than this are too generic to link files.
const MAX_DEF_FILES: usize = 40;
const DAMPING: f64 = 0.85;
const ITERATIONS: usize = 40;

/// Words that are never meaningful symbol names, across the outline languages.
const KEYWORDS: &[&str] = &[
    "abstract",
    "async",
    "await",
    "bool",
    "break",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "crate",
    "def",
    "default",
    "defer",
    "del",
    "dyn",
    "elif",
    "else",
    "enum",
    "except",
    "export",
    "extends",
    "extern",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "from",
    "func",
    "function",
    "go",
    "impl",
    "implements",
    "import",
    "int",
    "interface",
    "lambda",
    "let",
    "loop",
    "match",
    "mod",
    "module",
    "move",
    "mut",
    "new",
    "nil",
    "none",
    "None",
    "not",
    "null",
    "package",
    "pass",
    "private",
    "protected",
    "pub",
    "public",
    "raise",
    "ref",
    "return",
    "self",
    "Self",
    "static",
    "str",
    "String",
    "struct",
    "super",
    "switch",
    "this",
    "throw",
    "trait",
    "true",
    "True",
    "False",
    "try",
    "type",
    "typeof",
    "unsafe",
    "use",
    "usize",
    "var",
    "void",
    "where",
    "while",
    "with",
    "yield",
    "Some",
    "Ok",
    "Err",
    "Option",
    "Result",
    "Vec",
    "Box",
];

/// Method and function names so common across codebases and standard
/// libraries that a mention says little about which definition is meant.
const COMMON_NAMES: &[&str] = &[
    "append",
    "apply",
    "args",
    "as_str",
    "build",
    "call",
    "clear",
    "clone",
    "close",
    "cmp",
    "config",
    "contains",
    "context",
    "copy",
    "count",
    "create",
    "data",
    "default",
    "delete",
    "error",
    "execute",
    "expect",
    "extend",
    "filter",
    "find",
    "fmt",
    "format",
    "from_str",
    "get",
    "handle",
    "hash",
    "init",
    "insert",
    "into",
    "is_empty",
    "items",
    "iter",
    "join",
    "key",
    "keys",
    "len",
    "list",
    "load",
    "main",
    "map",
    "name",
    "next",
    "open",
    "options",
    "params",
    "parse",
    "path",
    "pop",
    "process",
    "push",
    "read",
    "remove",
    "render",
    "request",
    "reset",
    "response",
    "result",
    "reverse",
    "run",
    "save",
    "send",
    "set",
    "setup",
    "show",
    "size",
    "sort",
    "split",
    "start",
    "state",
    "stop",
    "test",
    "to_string",
    "trim",
    "unwrap",
    "update",
    "value",
    "values",
    "write",
];

/// English words that also appear as file stems (`with.rs`, `from.rs`) but
/// in a task almost never mean that file.
const STOP_WORDS: &[&str] = &[
    "about", "after", "also", "back", "been", "before", "both", "call", "code", "does", "done",
    "each", "else", "even", "every", "file", "files", "from", "have", "here", "into", "just",
    "like", "make", "many", "more", "most", "much", "must", "need", "only", "other", "over",
    "same", "should", "some", "such", "than", "that", "their", "them", "then", "there", "these",
    "they", "this", "those", "very", "want", "were", "what", "when", "where", "which", "while",
    "will", "with", "work", "would", "your", "test", "tests", "main", "util", "utils", "lib",
    "index", "data", "type", "types", "error", "errors",
];

pub(crate) struct CodeGraph {
    paths: Vec<String>,
    by_path: HashMap<String, usize>,
    /// Per file: its declarations.
    defs: Vec<Vec<Symbol>>,
    /// Interned names: id → name, name → id.
    names: Vec<String>,
    name_id: HashMap<String, u32>,
    /// Name id → files defining it.
    def_files: Vec<Vec<usize>>,
    /// Name id → number of files mentioning it.
    ref_files: Vec<u32>,
    /// Per file: (name id, count) of mentions of names defined elsewhere,
    /// sorted by id.
    refs: Vec<Vec<(u32, u32)>>,
}

/// Bump when the outline or identifier rules change, so old caches are ignored.
const CACHE_VERSION: u32 = 3;

/// Per-file scan results, kept between builds (as JSON in `.kamili/cache/`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct GraphCache {
    version: u32,
    /// Identifier table: id → name.
    names: Vec<String>,
    #[serde(skip)]
    name_ix: HashMap<String, u32>,
    files: HashMap<String, CachedFile>,
    /// Files parsed by the last build (not saved).
    #[serde(skip)]
    last_parsed: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(from = "CachedFileJson", into = "CachedFileJson")]
#[derive(Clone)]
struct CachedFile {
    size: u64,
    modified: u64,
    fingerprint: String,
    symbols: Vec<Symbol>,
    /// (identifier id, count).
    idents: Vec<(u32, u32)>,
}

/// Compact on-disk form: symbols as `[line, name, signature]` and identifiers
/// as one flat `[id, count, id, count, …]` list. About half the size of the
/// plain form.
#[derive(Serialize, Deserialize)]
struct CachedFileJson {
    s: u64,
    m: u64,
    f: String,
    d: Vec<(usize, Option<String>, String)>,
    i: Vec<u32>,
}

impl From<CachedFileJson> for CachedFile {
    fn from(j: CachedFileJson) -> Self {
        Self {
            size: j.s,
            modified: j.m,
            fingerprint: j.f,
            symbols: j
                .d
                .into_iter()
                .map(|(line, name, signature)| Symbol {
                    line,
                    name,
                    signature,
                })
                .collect(),
            idents: j.i.chunks_exact(2).map(|p| (p[0], p[1])).collect(),
        }
    }
}

impl From<CachedFile> for CachedFileJson {
    fn from(c: CachedFile) -> Self {
        Self {
            s: c.size,
            m: c.modified,
            f: c.fingerprint,
            d: c.symbols
                .into_iter()
                .map(|s| (s.line, s.name, s.signature))
                .collect(),
            i: c.idents.into_iter().flat_map(|(a, b)| [a, b]).collect(),
        }
    }
}

impl GraphCache {
    /// A cache from JSON; empty if the text is unusable or from another version.
    pub fn from_json(text: &str) -> Self {
        match serde_json::from_str::<GraphCache>(text) {
            Ok(mut c) if c.version == CACHE_VERSION => {
                c.name_ix = c
                    .names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (n.clone(), i as u32))
                    .collect();
                c
            }
            _ => Self::default(),
        }
    }

    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string(&GraphCacheOut {
            version: CACHE_VERSION,
            names: &self.names,
            files: &self.files,
        })
        .unwrap_or_default();
        text.push('\n');
        text
    }

    /// How many files the last build had to parse (the rest came from cache).
    pub fn last_parsed(&self) -> usize {
        self.last_parsed
    }

    fn intern(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.name_ix.get(name) {
            return id;
        }
        self.names.push(name.to_string());
        let id = (self.names.len() - 1) as u32;
        self.name_ix.insert(name.to_string(), id);
        id
    }

    /// Rebuilds the identifier table once more than half of it is unused.
    fn compact_if_needed(&mut self) {
        let mut used = vec![false; self.names.len()];
        for f in self.files.values() {
            for (id, _) in &f.idents {
                used[*id as usize] = true;
            }
        }
        let live = used.iter().filter(|u| **u).count();
        if live * 2 >= self.names.len() {
            return;
        }
        let mut remap = vec![0u32; self.names.len()];
        let old = std::mem::take(&mut self.names);
        self.name_ix.clear();
        for (i, name) in old.into_iter().enumerate() {
            if used[i] {
                remap[i] = self.intern(&name);
            }
        }
        for f in self.files.values_mut() {
            for (id, _) in &mut f.idents {
                *id = remap[*id as usize];
            }
        }
    }
}

#[derive(Serialize)]
struct GraphCacheOut<'a> {
    version: u32,
    names: &'a [String],
    files: &'a HashMap<String, CachedFile>,
}

/// What to rank towards: files and names from a task, spec or brief.
#[derive(Debug, Clone, Default)]
pub(crate) struct Focus {
    files: HashSet<usize>,
    names: HashSet<u32>,
}

impl Focus {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.names.is_empty()
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn name_count(&self) -> usize {
        self.names.len()
    }
}

/// Result of ranking: file scores and symbol scores.
pub(crate) struct Ranking {
    pub file_rank: Vec<f64>,
    /// (file, name id) → score.
    symbols: HashMap<(usize, u32), f64>,
}

type Edges = Vec<Vec<(usize, f64)>>;
/// A file's declarations and its (identifier id, count) mentions.
type FileScan<'a> = (&'a [Symbol], &'a [(u32, u32)]);

fn identifiers(text: &str) -> impl Iterator<Item = &str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < bytes.len() {
            let c = bytes[i];
            if c.is_ascii_alphabetic() || c == b'_' {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                return Some(&text[start..i]);
            }
            // Skip numbers so `0x1f` doesn't yield `x1f`.
            if c.is_ascii_digit() {
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                continue;
            }
            i += 1;
        }
        None
    })
}

/// `mod x;` / `pub(crate) mod x {`: a module, not something files call.
fn is_module_decl(signature: &str) -> bool {
    let s = signature.trim_start();
    let s = s.strip_prefix("pub").map_or(s, |rest| {
        rest.trim_start()
            .strip_prefix('(')
            .and_then(|r| r.split_once(')'))
            .map_or(rest, |(_, after)| after)
    });
    s.trim_start().starts_with("mod ")
}

fn meaningful(name: &str) -> bool {
    name.len() >= 3 && !KEYWORDS.contains(&name)
}

impl CodeGraph {
    /// Scans every in-scope source file under `root` (in parallel).
    #[cfg(test)]
    pub fn build(root: &Path, scope: &Scope) -> Self {
        Self::build_cached(root, scope, &mut GraphCache::default())
    }

    /// Like `build`, but reuses `cache` for files that haven't changed (same
    /// size and modification time, or same contents) and updates it.
    pub fn build_cached(root: &Path, scope: &Scope, cache: &mut GraphCache) -> Self {
        let sources: Vec<FileEntry> = files(root, scope)
            .into_iter()
            .filter(|f| f.size <= MAX_SCAN_BYTES && supported(&f.path))
            .collect();

        // Files whose size or time changed are read; parse only if the
        // contents differ too (a copied tree has new times, same contents).
        enum Outcome {
            Same(u64),
            Parsed(String, Vec<Symbol>, HashMap<String, u32>),
            Unreadable,
        }
        let stale: Vec<&FileEntry> = sources
            .iter()
            .filter(|f| {
                !cache
                    .files
                    .get(&f.path)
                    .is_some_and(|c| c.size == f.size && c.modified == f.modified)
            })
            .collect();
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(16);
        let chunk = stale.len().div_ceil(threads).max(64);
        let known = &cache.files;
        let outcomes: Vec<(String, Outcome)> = std::thread::scope(|sc| {
            let handles: Vec<_> = stale
                .chunks(chunk)
                .map(|batch| {
                    sc.spawn(move || {
                        batch
                            .iter()
                            .map(|f| {
                                let Ok(text) = read_text(&root.join(&f.path)) else {
                                    return (f.path.clone(), Outcome::Unreadable);
                                };
                                let print = fingerprint(text.as_bytes());
                                if known.get(&f.path).is_some_and(|c| c.fingerprint == print) {
                                    return (f.path.clone(), Outcome::Same(f.modified));
                                }
                                let mut counts: HashMap<String, u32> = HashMap::new();
                                for id in identifiers(&text).filter(|id| meaningful(id)) {
                                    *counts.entry(id.to_string()).or_default() += 1;
                                }
                                // The graph uses named declarations only.
                                let symbols: Vec<Symbol> = outline(&f.path, &text)
                                    .into_iter()
                                    .filter(|s| {
                                        !is_module_decl(&s.signature)
                                            && s.name.as_deref().is_some_and(meaningful)
                                    })
                                    .collect();
                                (f.path.clone(), Outcome::Parsed(print, symbols, counts))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect()
        });
        let parsed_now = outcomes
            .iter()
            .filter(|(_, o)| matches!(o, Outcome::Parsed(..)))
            .count();
        for (path, outcome) in outcomes {
            match outcome {
                Outcome::Same(modified) => {
                    if let Some(c) = cache.files.get_mut(&path) {
                        c.modified = modified;
                    }
                }
                Outcome::Parsed(print, symbols, counts) => {
                    let size = sources
                        .iter()
                        .find(|f| f.path == path)
                        .map_or(0, |f| f.size);
                    let modified = sources
                        .iter()
                        .find(|f| f.path == path)
                        .map_or(0, |f| f.modified);
                    let idents = counts
                        .into_iter()
                        .map(|(name, c)| (cache.intern(&name), c))
                        .collect();
                    cache.files.insert(
                        path,
                        CachedFile {
                            size,
                            modified,
                            fingerprint: print,
                            symbols,
                            idents,
                        },
                    );
                }
                Outcome::Unreadable => {
                    cache.files.remove(&path);
                }
            }
        }
        // Forget files that are gone, and drop unused names now and then.
        let live: HashSet<&str> = sources.iter().map(|f| f.path.as_str()).collect();
        cache.files.retain(|p, _| live.contains(p.as_str()));
        cache.last_parsed = parsed_now;

        let sources: Vec<String> = sources
            .into_iter()
            .map(|f| f.path)
            .filter(|p| cache.files.contains_key(p))
            .collect();
        let parsed: Vec<FileScan<'_>> = sources
            .iter()
            .map(|p| {
                let c = &cache.files[p];
                (c.symbols.as_slice(), c.idents.as_slice())
            })
            .collect();

        // Intern every defined name.
        let mut names: Vec<String> = Vec::new();
        let mut name_id: HashMap<String, u32> = HashMap::new();
        let mut def_files: Vec<Vec<usize>> = Vec::new();
        for (f, (symbols, _)) in parsed.iter().enumerate() {
            for name in symbols
                .iter()
                .filter(|s| !is_module_decl(&s.signature))
                .filter_map(|s| s.name.as_deref())
            {
                if !meaningful(name) {
                    continue;
                }
                let id = *name_id.entry(name.to_string()).or_insert_with(|| {
                    names.push(name.to_string());
                    def_files.push(Vec::new());
                    (names.len() - 1) as u32
                });
                let list = &mut def_files[id as usize];
                if list.last() != Some(&f) {
                    list.push(f);
                }
            }
        }
        // Cache name id → graph name id, for names the project defines.
        let to_graph: Vec<Option<u32>> = cache
            .names
            .iter()
            .map(|n| name_id.get(n).copied())
            .collect();
        let mut ref_files = vec![0u32; names.len()];
        let mut defs = Vec::with_capacity(parsed.len());
        let mut refs = Vec::with_capacity(parsed.len());
        for (symbols, idents) in parsed {
            let mut mine: Vec<(u32, u32)> = idents
                .iter()
                .filter_map(|&(cid, c)| to_graph[cid as usize].map(|id| (id, c)))
                .collect();
            mine.sort_unstable();
            for (id, _) in &mine {
                ref_files[*id as usize] += 1;
            }
            defs.push(symbols.to_vec());
            refs.push(mine);
        }
        // Only names the project declares can link files, so the cache keeps
        // just those (a much smaller file). A file that starts using a newly
        // declared name links to it once that file changes.
        for f in cache.files.values_mut() {
            f.idents
                .retain(|(cid, _)| to_graph[*cid as usize].is_some());
        }
        cache.compact_if_needed();
        let by_path = sources
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), i))
            .collect();
        Self {
            paths: sources,
            by_path,
            defs,
            names,
            name_id,
            def_files,
            ref_files,
            refs,
        }
    }

    pub fn file_count(&self) -> usize {
        self.paths.len()
    }

    pub fn symbol_count(&self) -> usize {
        self.defs.iter().map(Vec::len).sum()
    }

    fn mentions(&self, file: usize, id: u32) -> bool {
        self.refs[file]
            .binary_search_by_key(&id, |(n, _)| *n)
            .is_ok()
    }

    /// How much one mention of a name links the mentioning file to the files
    /// defining it. 0 for names too generic to mean anything.
    fn name_weight(&self, id: u32) -> f64 {
        let name = &self.names[id as usize];
        let defined_in = self.def_files[id as usize].len();
        if defined_in > MAX_DEF_FILES {
            return 0.0;
        }
        let mut w = 1.0 / defined_in as f64;
        let distinctive = name.len() >= 8
            && (name.contains('_') || name.chars().skip(1).any(|c| c.is_ascii_uppercase()));
        if distinctive {
            w *= 3.0;
        }
        if name.starts_with('_') || COMMON_NAMES.contains(&name.as_str()) {
            w *= 0.1;
        }
        // Names nearly everyone mentions (std macros, common helpers) mostly
        // refer to something outside the project: like stop words, they say
        // little about which file depends on which.
        let users = f64::from(self.ref_files[id as usize]);
        let common = (self.paths.len() as f64 * 0.05).max(10.0);
        if users > common {
            w *= (common / users).powi(2);
        }
        w
    }

    /// Focus from free text (task, spec, brief) and explicit paths. Picks up
    /// names the project defines, file paths, and file stems ("greet" → greet.rs).
    pub fn focus(&self, text: &str, paths: &[String]) -> Focus {
        let mut focus = Focus::default();
        let mut words: HashSet<String> = HashSet::new();
        for id in identifiers(text) {
            if let Some(&n) = self.name_id.get(id)
                && self.name_weight(n) > 0.0
            {
                focus.names.insert(n);
            }
            if id.len() >= 4 {
                words.insert(id.to_ascii_lowercase());
            }
        }
        words.retain(|w| !STOP_WORDS.contains(&w.as_str()));
        // Module name: the file stem, or the directory for `mod.rs`,
        // `index.ts`, `__init__.py` and similar. Plurals match singulars.
        let stem_of = |path: &str| {
            let p = Path::new(path);
            let stem = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            let stem = if matches!(stem.as_str(), "mod" | "index" | "__init__" | "lib" | "main") {
                p.parent()
                    .and_then(Path::file_name)
                    .map(|d| d.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or(stem)
            } else {
                stem
            };
            stem.strip_suffix('s')
                .filter(|s| s.len() >= 4)
                .map(str::to_string)
                .unwrap_or(stem)
        };
        let words: HashSet<String> = words
            .into_iter()
            .map(|w| {
                w.strip_suffix('s')
                    .filter(|s| s.len() >= 4)
                    .map(str::to_string)
                    .unwrap_or(w)
            })
            .collect();
        // A stem shared by many files ("parse.rs" in ten crates) says too
        // little about which one is meant.
        let mut stem_count: HashMap<String, usize> = HashMap::new();
        for path in &self.paths {
            let stem = stem_of(path);
            if words.contains(&stem) {
                *stem_count.entry(stem).or_default() += 1;
            }
        }
        for (i, path) in self.paths.iter().enumerate() {
            let stem = stem_of(path);
            let by_stem = stem.len() >= 4 && stem_count.get(&stem).is_some_and(|&n| n <= 5);
            if text.contains(path.as_str()) || paths.contains(path) || by_stem {
                focus.files.insert(i);
            }
        }
        for &n in &focus.names {
            if self.def_files[n as usize].len() <= 5 {
                focus.files.extend(&self.def_files[n as usize]);
            }
        }
        focus
    }

    /// Weighted edges A → B, and (B, name) → incoming weight.
    fn edges(&self, focus: &Focus) -> (Edges, HashMap<(usize, u32), f64>) {
        let weights: Vec<f64> = (0..self.names.len() as u32)
            .map(|id| {
                let w = self.name_weight(id);
                if focus.names.contains(&id) {
                    w * 10.0
                } else {
                    w
                }
            })
            .collect();
        let mut out = vec![Vec::new(); self.paths.len()];
        let mut incoming: HashMap<(usize, u32), f64> = HashMap::new();
        for (a, mentions) in self.refs.iter().enumerate() {
            let mut targets: BTreeMap<usize, f64> = BTreeMap::new();
            for &(id, count) in mentions {
                let w = weights[id as usize] * f64::from(count).sqrt();
                if w == 0.0 {
                    continue;
                }
                for &b in self.def_files[id as usize].iter().filter(|&&b| b != a) {
                    *targets.entry(b).or_default() += w;
                    *incoming.entry((b, id)).or_default() += w;
                }
            }
            out[a] = targets.into_iter().collect();
        }
        (out, incoming)
    }

    /// PageRank, personalised towards `focus` when it is non-empty.
    pub fn rank(&self, focus: &Focus) -> Ranking {
        let n = self.paths.len();
        if n == 0 {
            return Ranking {
                file_rank: Vec::new(),
                symbols: HashMap::new(),
            };
        }
        let (out, incoming) = self.edges(focus);
        let mut p = vec![1.0 / n as f64; n];
        if !focus.files.is_empty() {
            // Nearly all restart mass on the focus files.
            let share = 0.97 / focus.files.len() as f64;
            for (i, v) in p.iter_mut().enumerate() {
                *v = 0.03 / n as f64 + if focus.files.contains(&i) { share } else { 0.0 };
            }
        }
        let out_sum: Vec<f64> = out.iter().map(|e| e.iter().map(|(_, w)| w).sum()).collect();
        let mut r = p.clone();
        for _ in 0..ITERATIONS {
            let mut next: Vec<f64> = p.iter().map(|v| (1.0 - DAMPING) * v).collect();
            let mut dangling = 0.0;
            for a in 0..n {
                if out_sum[a] == 0.0 {
                    dangling += r[a];
                    continue;
                }
                let scale = DAMPING * r[a] / out_sum[a];
                for &(b, w) in &out[a] {
                    next[b] += scale * w;
                }
            }
            for (v, pv) in next.iter_mut().zip(&p) {
                *v += DAMPING * dangling * pv;
            }
            r = next;
        }
        // What the task is about comes first, then what that depends on.
        let top = r.iter().copied().fold(0.0, f64::max);
        for &f in &focus.files {
            r[f] += top;
        }

        // Share each file's rank among its symbols by incoming weight; symbols
        // nobody references get a small equal share.
        let mut in_total = vec![0.0; n];
        for ((b, _), w) in &incoming {
            in_total[*b] += w;
        }
        let mut symbols = HashMap::new();
        for (b, defs) in self.defs.iter().enumerate() {
            for name in defs.iter().filter_map(|s| s.name.as_deref()) {
                let Some(&id) = self.name_id.get(name) else {
                    continue;
                };
                let w = incoming.get(&(b, id)).copied().unwrap_or(0.0);
                let share = if in_total[b] > 0.0 {
                    0.9 * w / in_total[b] + 0.1 / defs.len() as f64
                } else {
                    1.0 / defs.len() as f64
                };
                let boost = if focus.names.contains(&id) { 2.0 } else { 1.0 };
                symbols.insert((b, id), r[b] * share * boost);
            }
        }
        Ranking {
            file_rank: r,
            symbols,
        }
    }

    fn symbol_score(&self, ranking: &Ranking, file: usize, s: &Symbol) -> f64 {
        s.name
            .as_deref()
            .and_then(|n| self.name_id.get(n))
            .and_then(|id| ranking.symbols.get(&(file, *id)))
            .copied()
            .unwrap_or(0.0)
    }

    /// The most relevant files with their most relevant declarations, within
    /// `budget` bytes. This is the ranked part of the repo map.
    pub fn ranked_map(&self, ranking: &Ranking, budget: usize) -> String {
        let mut syms: Vec<(&(usize, u32), &f64)> = ranking.symbols.iter().collect();
        syms.sort_by(|a, b| b.1.total_cmp(a.1).then_with(|| a.0.cmp(b.0)));
        // Files in order of their best symbol.
        let mut order: Vec<usize> = Vec::new();
        let mut seen: HashSet<usize> = HashSet::new();
        for ((file, _), _) in &syms {
            if seen.insert(*file) {
                order.push(*file);
            }
        }
        let mut out = String::new();
        let mut per_file_cap = 12;
        loop {
            out.clear();
            for &f in &order {
                let mut block = format!("{}\n", self.paths[f]);
                let mut ranked: Vec<&Symbol> =
                    self.defs[f].iter().filter(|s| s.name.is_some()).collect();
                ranked.sort_by(|a, b| {
                    self.symbol_score(ranking, f, b)
                        .total_cmp(&self.symbol_score(ranking, f, a))
                });
                ranked.truncate(per_file_cap);
                ranked.sort_by_key(|s| s.line);
                for s in ranked {
                    let _ = writeln!(block, "{:>6}  {}", s.line, clip(&s.signature));
                }
                if out.len() + block.len() > budget {
                    break;
                }
                out.push_str(&block);
            }
            // Prefer more files with fewer symbols each over a few long lists.
            let files_shown = out.lines().filter(|l| !l.starts_with(' ')).count();
            if per_file_cap <= 3 || files_shown >= order.len().min(30) {
                break;
            }
            per_file_cap /= 2;
        }
        out
    }

    /// Where a name is defined: `path:line: declaration`.
    pub fn definitions(&self, name: &str) -> String {
        let mut out = Vec::new();
        if let Some(&id) = self.name_id.get(name) {
            for &f in &self.def_files[id as usize] {
                for s in self.defs[f]
                    .iter()
                    .filter(|s| s.name.as_deref() == Some(name))
                {
                    out.push(format!("{}:{}: {}", self.paths[f], s.line, s.signature));
                }
            }
        }
        if out.is_empty() {
            format!("no definition of `{name}` found; try grep")
        } else {
            out.join("\n")
        }
    }

    /// Lines mentioning `name` as a whole identifier, outside its definitions.
    pub fn references(&self, root: &Path, name: &str) -> String {
        let Some(&id) = self.name_id.get(name) else {
            return format!("`{name}` is not defined in the project; try grep");
        };
        let def_lines: HashSet<(usize, usize)> = self.def_files[id as usize]
            .iter()
            .flat_map(|&f| {
                self.defs[f]
                    .iter()
                    .filter(|s| s.name.as_deref() == Some(name))
                    .map(move |s| (f, s.line))
            })
            .collect();
        let mut out = Vec::new();
        let mut total = 0;
        for f in (0..self.paths.len()).filter(|&f| self.mentions(f, id)) {
            let Ok(text) = read_text(&root.join(&self.paths[f])) else {
                continue;
            };
            for (i, line) in text.lines().enumerate() {
                if def_lines.contains(&(f, i + 1)) || !identifiers(line).any(|w| w == name) {
                    continue;
                }
                total += 1;
                if out.len() < MAX_MATCHES {
                    out.push(format!(
                        "{}:{}: {}",
                        self.paths[f],
                        i + 1,
                        clip(line.trim())
                    ));
                }
            }
        }
        if total > out.len() {
            out.push(format!("… {} more references", total - out.len()));
        }
        if out.is_empty() {
            format!("no references to `{name}` outside its definitions")
        } else {
            out.join("\n")
        }
    }

    /// Names that link `from` to `to`, for display.
    fn linking_names(&self, from: usize, to: usize) -> String {
        let mut names: Vec<&str> = self.refs[from]
            .iter()
            .filter(|(id, _)| {
                self.def_files[*id as usize].contains(&to) && self.name_weight(*id) >= 0.2
            })
            .map(|(id, _)| self.names[*id as usize].as_str())
            .collect();
        names.sort();
        let more = names.len().saturating_sub(6);
        names.truncate(6);
        let mut s = names.join(", ");
        if more > 0 {
            let _ = write!(s, ", +{more}");
        }
        s
    }

    /// What a file uses (its dependencies) and what uses it, with the names
    /// that connect them, strongest first.
    pub fn related(&self, path: &str) -> Option<String> {
        let &f = self.by_path.get(path.trim_start_matches("./"))?;
        let (out, _) = self.edges(&Focus::default());
        let mut uses: Vec<(usize, f64)> = out[f].clone();
        let mut used_by: Vec<(usize, f64)> = out
            .iter()
            .enumerate()
            .filter_map(|(a, e)| e.iter().find(|(b, _)| *b == f).map(|(_, w)| (a, *w)))
            .collect();
        uses.sort_by(|a, b| b.1.total_cmp(&a.1));
        used_by.sort_by(|a, b| b.1.total_cmp(&a.1));
        // Keep only links made through specific names.
        uses.retain(|(b, _)| !self.linking_names(f, *b).is_empty());
        used_by.retain(|(a, _)| !self.linking_names(*a, f).is_empty());
        let mut s = format!("{path}\n\nuses ({}):\n", uses.len());
        for (b, _) in uses.iter().take(25) {
            let _ = writeln!(s, "  {}  ({})", self.paths[*b], self.linking_names(f, *b));
        }
        let _ = write!(s, "\nused by ({}):\n", used_by.len());
        for (a, _) in used_by.iter().take(25) {
            let _ = writeln!(s, "  {}  ({})", self.paths[*a], self.linking_names(*a, f));
        }
        Some(s)
    }

    /// Files (other than `changed`) that mention names defined in `changed`,
    /// strongest first, with the names that link them. A cheap estimate of
    /// what a change could affect; weak, generic links are left out.
    pub fn impact(&self, changed: &[String]) -> Vec<(String, Vec<String>)> {
        let changed_ids: HashSet<usize> = changed
            .iter()
            .filter_map(|p| self.by_path.get(p).copied())
            .collect();
        let names: HashSet<u32> = changed_ids
            .iter()
            .flat_map(|&f| self.defs[f].iter().filter_map(|s| s.name.as_deref()))
            .filter_map(|n| self.name_id.get(n).copied())
            .filter(|&id| self.name_weight(id) > 0.0)
            .collect();
        let mut hits: Vec<(usize, f64, Vec<String>)> = (0..self.paths.len())
            .filter(|f| !changed_ids.contains(f))
            .filter_map(|f| {
                let linked: Vec<&(u32, u32)> = self.refs[f]
                    .iter()
                    .filter(|(id, _)| names.contains(id))
                    .collect();
                let w: f64 = linked
                    .iter()
                    .map(|(id, c)| self.name_weight(*id) * f64::from(*c).sqrt())
                    .sum();
                let mut via: Vec<String> = linked
                    .iter()
                    .filter(|(id, _)| self.name_weight(*id) >= 0.2)
                    .map(|(id, _)| self.names[*id as usize].clone())
                    .collect();
                via.sort();
                (w >= 0.3 && !via.is_empty()).then_some((f, w, via))
            })
            .collect();
        hits.sort_by(|a, b| b.1.total_cmp(&a.1));
        hits.into_iter()
            .map(|(f, _, via)| (self.paths[f].clone(), via))
            .collect()
    }

    /// Graphviz DOT of the `limit` highest-ranked files and the edges between them.
    pub fn to_dot(&self, ranking: &Ranking, limit: usize) -> String {
        let top = self.top_files(ranking, limit);
        let keep: HashSet<usize> = top.iter().map(|(f, _)| *f).collect();
        let (out, _) = self.edges(&Focus::default());
        let mut s =
            String::from("digraph code {\n  rankdir=LR;\n  node [shape=box, fontsize=10];\n");
        let max = top.first().map_or(1.0, |(_, r)| *r);
        for (f, r) in &top {
            let _ = writeln!(
                s,
                "  n{f} [label=\"{}\", penwidth={:.1}];",
                self.paths[*f].replace('"', "'"),
                1.0 + 3.0 * r / max
            );
        }
        let mut kept: Vec<usize> = keep.iter().copied().collect();
        kept.sort_unstable();
        for a in kept {
            for &(b, w) in &out[a] {
                if keep.contains(&b) {
                    let _ = writeln!(
                        s,
                        "  n{a} -> n{b} [penwidth={:.1}];",
                        (1.0 + w.ln_1p()).min(5.0)
                    );
                }
            }
        }
        s.push_str("}\n");
        s
    }

    /// `(file, rank)` for the `limit` highest-ranked files.
    pub fn top_files(&self, ranking: &Ranking, limit: usize) -> Vec<(usize, f64)> {
        let mut all: Vec<(usize, f64)> = ranking.file_rank.iter().copied().enumerate().collect();
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        all.truncate(limit);
        all
    }

    pub fn path(&self, file: usize) -> &str {
        &self.paths[file]
    }

    /// A file's declared names, most important first.
    pub fn top_symbols(&self, ranking: &Ranking, file: usize, limit: usize) -> Vec<String> {
        let mut named: Vec<(&str, f64)> = self.defs[file]
            .iter()
            .filter_map(|s| {
                s.name
                    .as_deref()
                    .map(|n| (n, self.symbol_score(ranking, file, s)))
            })
            .collect();
        named.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut seen = HashSet::new();
        named
            .into_iter()
            .filter(|(n, _)| seen.insert(*n))
            .take(limit)
            .map(|(n, _)| n.to_string())
            .collect()
    }

    /// Number of distinct file → file edges.
    pub fn edge_count(&self) -> usize {
        self.edges(&Focus::default()).0.iter().map(Vec::len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// app.rs is used by main.rs and cli.rs; config.rs by app.rs and cli.rs;
    /// util.rs by nobody.
    fn project() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let w = |p: &str, s: &str| {
            let path = d.path().join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, s).unwrap();
        };
        w(
            "src/main.rs",
            "fn main() {\n    run_application(load_settings());\n}\n",
        );
        w(
            "src/app.rs",
            "pub fn run_application(s: Settings) {\n    let c = load_settings();\n}\npub fn shutdown_hooks() {}\n",
        );
        w(
            "src/config.rs",
            "pub struct Settings;\npub fn load_settings() -> Settings { Settings }\n",
        );
        w(
            "src/cli.rs",
            "fn cli() {\n    run_application(load_settings());\n    shutdown_hooks();\n}\n",
        );
        w(
            "src/util.rs",
            "pub fn password_reset_token() -> u32 { 0 }\n",
        );
        w("README.md", "load_settings is documented here\n");
        d
    }

    #[test]
    fn builds_and_ranks_central_files_first() {
        let d = project();
        let g = CodeGraph::build(d.path(), &Scope::default());
        assert_eq!(g.file_count(), 5, "markdown is not a source file");
        let r = g.rank(&Focus::default());
        let top: Vec<&str> = g.top_files(&r, 2).iter().map(|(f, _)| g.path(*f)).collect();
        assert_eq!(top, ["src/config.rs", "src/app.rs"]);
        assert_eq!(
            g.top_symbols(&r, g.by_path["src/app.rs"], 1),
            ["run_application"]
        );
        let map = g.ranked_map(&r, 10_000);
        assert!(map.starts_with("src/config.rs\n"), "{map}");
        assert!(
            map.contains("     1  pub fn run_application(s: Settings)"),
            "{map}"
        );
    }

    #[test]
    fn focus_pulls_matching_files_up() {
        let d = project();
        let g = CodeGraph::build(d.path(), &Scope::default());
        let focus = g.focus("add expiry to password_reset_token", &[]);
        let r = g.rank(&focus);
        assert_eq!(g.path(g.top_files(&r, 1)[0].0), "src/util.rs");
        // Module files are matched by their directory; plurals match.
        let d2 = tempfile::tempdir().unwrap();
        fs::create_dir_all(d2.path().join("src/agents")).unwrap();
        fs::write(
            d2.path().join("src/agents/mod.rs"),
            "pub fn spawn_worker() {}\n",
        )
        .unwrap();
        fs::write(d2.path().join("src/tools.rs"), "pub fn list_callers() {}\n").unwrap();
        let g2 = CodeGraph::build(d2.path(), &Scope::default());
        assert_eq!(g2.focus("give the agent a new tool", &[]).file_count(), 2);
        // File stems count too: "the cli needs a flag" → src/cli.rs.
        let focus = g.focus("the cli needs a --verbose flag", &[]);
        assert!(focus.files.contains(&g.by_path["src/cli.rs"]));
    }

    #[test]
    fn lookups_related_and_impact() {
        let d = project();
        let g = CodeGraph::build(d.path(), &Scope::default());
        assert_eq!(
            g.definitions("load_settings"),
            "src/config.rs:2: pub fn load_settings() -> Settings { Settings }"
        );
        let refs = g.references(d.path(), "load_settings");
        assert_eq!(refs.lines().count(), 3, "{refs}");
        assert!(
            !refs.contains("src/config.rs:2"),
            "definition excluded: {refs}"
        );
        let rel = g.related("src/app.rs").unwrap();
        assert!(
            rel.contains("uses (1):\n  src/config.rs  (Settings, load_settings)"),
            "{rel}"
        );
        assert!(
            rel.contains("src/cli.rs  (run_application, shutdown_hooks)"),
            "{rel}"
        );
        assert!(g.related("nope.rs").is_none());
        let impact: Vec<String> = g
            .impact(&["src/config.rs".into()])
            .into_iter()
            .map(|(p, via)| format!("{p} ({})", via.join(", ")))
            .collect();
        assert_eq!(
            impact,
            [
                "src/app.rs (Settings, load_settings)",
                "src/cli.rs (load_settings)",
                "src/main.rs (load_settings)"
            ]
        );
        let r = g.rank(&Focus::default());
        let dot = g.to_dot(&r, 3);
        assert!(
            dot.starts_with("digraph code {") && dot.contains("->"),
            "{dot}"
        );
    }

    #[test]
    fn cache_reuses_unchanged_files_and_survives_json() {
        let d = project();
        let mut cache = GraphCache::default();
        let g1 = CodeGraph::build_cached(d.path(), &Scope::default(), &mut cache);
        assert_eq!(cache.last_parsed(), 5);

        // Round trip, then rebuild: nothing to parse.
        let mut cache = GraphCache::from_json(&cache.to_json());
        let g2 = CodeGraph::build_cached(d.path(), &Scope::default(), &mut cache);
        assert_eq!(cache.last_parsed(), 0);
        assert_eq!(
            g1.definitions("load_settings"),
            g2.definitions("load_settings")
        );

        // Edit one file and delete another: one parse, the deleted file is gone.
        fs::write(
            d.path().join("src/util.rs"),
            "pub fn new_token_helper() {}\n",
        )
        .unwrap();
        fs::remove_file(d.path().join("src/cli.rs")).unwrap();
        let g3 = CodeGraph::build_cached(d.path(), &Scope::default(), &mut cache);
        assert_eq!(cache.last_parsed(), 1);
        assert_eq!(g3.file_count(), 4);
        assert!(
            g3.definitions("new_token_helper")
                .starts_with("src/util.rs:1")
        );

        // A copied tree (new times, same contents) reuses entries.
        let copy = tempfile::tempdir().unwrap();
        for f in ["src/main.rs", "src/app.rs", "src/config.rs", "src/util.rs"] {
            fs::create_dir_all(copy.path().join("src")).unwrap();
            fs::copy(d.path().join(f), copy.path().join(f)).unwrap();
        }
        CodeGraph::build_cached(copy.path(), &Scope::default(), &mut cache);
        assert_eq!(cache.last_parsed(), 0);

        // Wrong version or garbage: start empty.
        assert!(GraphCache::from_json("{\"version\":0}").files.is_empty());
        assert!(GraphCache::from_json("not json").files.is_empty());
    }

    #[test]
    fn module_declarations_are_not_symbols() {
        assert!(is_module_decl("mod tests"));
        assert!(is_module_decl("pub(crate) mod engine;"));
        assert!(is_module_decl("pub mod api"));
        assert!(!is_module_decl("pub fn module_path()"));
    }

    #[test]
    fn identifiers_skip_numbers_and_keywords() {
        let ids: Vec<&str> = identifiers("let x1 = 0x1f + foo_bar(Baz);").collect();
        assert_eq!(ids, ["let", "x1", "foo_bar", "Baz"]);
        assert!(!meaningful("let") && !meaningful("x1") && meaningful("foo_bar"));
    }
}
