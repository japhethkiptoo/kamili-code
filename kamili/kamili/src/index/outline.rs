//! Lightweight symbol outlines by regex: top-level and indented declarations
//! for common languages. Approximate by design: a cheap map for an agent to
//! decide which lines to read, not a parser.

use std::sync::OnceLock;

use regex::Regex;

struct Lang {
    exts: &'static [&'static str],
    patterns: &'static [&'static str],
}

const LANGS: &[Lang] = &[
    Lang {
        exts: &["rs"],
        patterns: &[
            r"^\s*(pub(\([^)]*\))?\s+)?(async\s+)?(const\s+)?(unsafe\s+)?fn\s+\w+",
            r"^\s*(pub(\([^)]*\))?\s+)?(struct|enum|trait|union|type|mod)\s+\w+",
            r"^\s*impl\b.*",
            r"^\s*macro_rules!\s*\w+",
        ],
    },
    Lang {
        exts: &["py"],
        patterns: &[r"^\s*(async\s+)?def\s+\w+", r"^\s*class\s+\w+"],
    },
    Lang {
        exts: &["js", "jsx", "ts", "tsx", "mjs", "cjs"],
        patterns: &[
            r"^\s*(export\s+)?(default\s+)?(async\s+)?function\*?\s+\w+",
            r"^\s*(export\s+)?(default\s+)?(abstract\s+)?class\s+\w+",
            r"^\s*(export\s+)?(interface|type|enum)\s+\w+",
            r"^\s*(export\s+)?const\s+\w+\s*=\s*(async\s*)?(\([^)]*\)|\w+)\s*=>",
        ],
    },
    Lang {
        exts: &["go"],
        patterns: &[r"^func\s+(\([^)]*\)\s*)?\w+", r"^type\s+\w+"],
    },
    Lang {
        exts: &["java", "kt", "kts", "scala", "cs"],
        patterns: &[
            r"^\s*((public|private|protected|internal|static|final|abstract|sealed|open|data|override|suspend|async|partial)\s+)*(class|interface|enum|record|object|struct)\s+\w+",
            r"^\s*((public|private|protected|internal|static|final|abstract|override|suspend|async|virtual)\s+)+[\w<>\[\],? ]+\s+\w+\s*\(",
            r"^\s*(fun|def)\s+\w+",
        ],
    },
    Lang {
        exts: &["rb"],
        patterns: &[r"^\s*(def|class|module)\s+[\w.:]+"],
    },
    Lang {
        exts: &["php"],
        patterns: &[
            r"^\s*((public|private|protected|static|abstract|final)\s+)*(function|class|interface|trait|enum)\s+\w+",
        ],
    },
    Lang {
        exts: &["swift"],
        patterns: &[
            r"^\s*((public|private|internal|fileprivate|open|static|final)\s+)*(func|class|struct|enum|protocol|extension)\s+\w+",
        ],
    },
    Lang {
        exts: &["c", "h", "cc", "cpp", "hpp", "cxx"],
        patterns: &[
            r"^(struct|class|enum|union|typedef|namespace)\s+\w+",
            r"^[A-Za-z_][\w\s\*&:<>,]*\s+\**[A-Za-z_][\w:]*\s*\([^;]*$",
        ],
    },
];

fn compiled() -> &'static Vec<(Vec<&'static str>, Vec<Regex>)> {
    static CELL: OnceLock<Vec<(Vec<&'static str>, Vec<Regex>)>> = OnceLock::new();
    CELL.get_or_init(|| {
        LANGS
            .iter()
            .map(|l| {
                (
                    l.exts.to_vec(),
                    l.patterns
                        .iter()
                        .map(|p| Regex::new(p).expect("outline pattern"))
                        .collect(),
                )
            })
            .collect()
    })
}

/// One declaration found in a file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Symbol {
    /// 1-based.
    pub line: usize,
    /// The declared name, when one could be extracted (`impl` blocks have none).
    pub name: Option<String>,
    /// The trimmed declaration line.
    pub signature: String,
}

/// Whether outlines (and so the code graph) cover this file's language.
pub(crate) fn supported(path: &str) -> bool {
    patterns_for(path).is_some()
}

fn patterns_for(path: &str) -> Option<&'static Vec<Regex>> {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    compiled()
        .iter()
        .find(|(exts, _)| exts.contains(&ext.as_str()))
        .map(|(_, p)| p)
}

/// Declared name: the identifier after a declaring keyword (Go receivers
/// skipped), else the identifier before the first `(` (Java/C-style methods).
fn declared_name(line: &str) -> Option<String> {
    static KEYWORD: OnceLock<Regex> = OnceLock::new();
    static CALL: OnceLock<Regex> = OnceLock::new();
    let keyword = KEYWORD.get_or_init(|| {
        Regex::new(
            r"\b(?:fn|struct|enum|trait|union|type|mod|macro_rules!|class|def|function\*?|interface|func|fun|record|object|module|protocol|extension|namespace|const)\s*(?:\([^)]*\)\s*)?([A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("name pattern")
    });
    let call =
        CALL.get_or_init(|| Regex::new(r"([A-Za-z_][A-Za-z0-9_]*)\s*\(").expect("call pattern"));
    if line.trim_start().starts_with("impl") {
        return None;
    }
    keyword
        .captures(line)
        .or_else(|| call.captures(line))
        .map(|c| c[1].to_string())
}

/// The declarations in a file, in order.
pub(crate) fn outline(path: &str, text: &str) -> Vec<Symbol> {
    let Some(patterns) = patterns_for(path) else {
        return Vec::new();
    };
    text.lines()
        .enumerate()
        .filter(|(_, line)| patterns.iter().any(|re| re.is_match(line)))
        .map(|(i, line)| {
            let sig = line.trim().trim_end_matches('{').trim_end();
            Symbol {
                line: i + 1,
                name: declared_name(line),
                signature: sig.chars().take(160).collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_python_ts_go() {
        let rs = "pub struct A;\nimpl A {\n    pub async fn run(&self) {}\n}\nfn helper() {}\n// fn not_code\n";
        let syms = outline("x.rs", rs);
        let lines: Vec<usize> = syms.iter().map(|s| s.line).collect();
        assert_eq!(lines, [1, 2, 3, 5]);
        let names: Vec<Option<&str>> = syms.iter().map(|s| s.name.as_deref()).collect();
        assert_eq!(names, [Some("A"), None, Some("run"), Some("helper")]);
        assert_eq!(
            outline("a.py", "class A:\n    def f(self):\n        pass\n").len(),
            2
        );
        let ts = "export const add = (a, b) => a + b;\nexport interface X {}\nconst y = 1;\n";
        assert_eq!(outline("a.ts", ts).len(), 2);
        assert_eq!(
            outline("m.go", "func (s *S) Run() {}\ntype S struct{}\n").len(),
            2
        );
        assert!(outline("notes.txt", "fn x").is_empty());
        let go = outline("m.go", "func (s *S) Run() {}\n");
        assert_eq!(go[0].name.as_deref(), Some("Run"));
        let java = outline("A.java", "    public static int parseArgs(String[] a) {\n");
        assert_eq!(java[0].name.as_deref(), Some("parseArgs"));
        let ts = outline("a.ts", "export const add = (a, b) => a + b;\n");
        assert_eq!(ts[0].name.as_deref(), Some("add"));
    }
}
