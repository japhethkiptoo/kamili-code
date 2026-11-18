//! The tools agents may call, and the trait the engine implements to run them.
//!
//! Agents only *request* tool calls. The engine decides what each one does
//! (which root, which paths are allowed) in its [`ToolHost`] implementation.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::{ToolCall, ToolDef};

/// A parsed tool call.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "name", content = "input", rename_all = "snake_case")]
pub(crate) enum ToolRequest {
    ListDir {
        #[serde(default = "root")]
        path: String,
    },
    FindFiles {
        pattern: String,
    },
    Grep {
        pattern: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        glob: Option<String>,
        #[serde(default)]
        case_insensitive: bool,
    },
    ReadFile {
        path: String,
        #[serde(default)]
        start_line: Option<usize>,
        #[serde(default)]
        end_line: Option<usize>,
    },
    Outline {
        path: String,
    },
    FindSymbol {
        name: String,
    },
    References {
        name: String,
    },
    RelatedFiles {
        path: String,
    },
    WriteFile {
        path: String,
        content: String,
    },
    ReplaceInFile {
        path: String,
        old_text: String,
        new_text: String,
        #[serde(default)]
        replace_all: bool,
    },
    DeleteFile {
        path: String,
    },
}

fn root() -> String {
    ".".into()
}

impl ToolRequest {
    pub fn parse(call: &ToolCall) -> Result<Self, String> {
        serde_json::from_value(json!({"name": call.name, "input": call.input}))
            .map_err(|e| format!("invalid call to `{}`: {e}", call.name))
    }

    pub fn is_write(&self) -> bool {
        matches!(
            self,
            ToolRequest::WriteFile { .. }
                | ToolRequest::ReplaceInFile { .. }
                | ToolRequest::DeleteFile { .. }
        )
    }

    /// Short description for progress output, e.g. `grep "fn main" in src`.
    pub fn describe(&self) -> String {
        match self {
            ToolRequest::ListDir { path } => format!("list {path}"),
            ToolRequest::FindFiles { pattern } => format!("find {pattern}"),
            ToolRequest::Grep { pattern, path, .. } => match path {
                Some(p) => format!("grep {pattern:?} in {p}"),
                None => format!("grep {pattern:?}"),
            },
            ToolRequest::ReadFile {
                path,
                start_line,
                end_line,
            } => match (start_line, end_line) {
                (None, None) => format!("read {path}"),
                (s, e) => format!(
                    "read {path}:{}-{}",
                    s.unwrap_or(1),
                    e.map_or("end".into(), |e| e.to_string())
                ),
            },
            ToolRequest::Outline { path } => format!("outline {path}"),
            ToolRequest::FindSymbol { name } => format!("find symbol {name}"),
            ToolRequest::References { name } => format!("references to {name}"),
            ToolRequest::RelatedFiles { path } => format!("related to {path}"),
            ToolRequest::WriteFile { path, .. } => format!("write {path}"),
            ToolRequest::ReplaceInFile { path, .. } => format!("edit {path}"),
            ToolRequest::DeleteFile { path } => format!("delete {path}"),
        }
    }
}

/// Runs tool calls for one agent turn. Implemented by the engine.
pub(crate) trait ToolHost: Send {
    /// `Ok` text or `Err` message, both sent back to the model.
    fn run(&mut self, request: &ToolRequest) -> Result<String, String>;
}

/// Which tools an agent gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolKit {
    ReadOnly,
    ReadWrite,
}

impl ToolKit {
    pub fn allows(self, request: &ToolRequest) -> bool {
        self == ToolKit::ReadWrite || !request.is_write()
    }

    pub fn defs(self) -> Vec<ToolDef> {
        let mut defs = read_tools();
        if self == ToolKit::ReadWrite {
            defs.extend(write_tools());
        }
        defs
    }
}

fn def(name: &str, description: &str, schema: Value) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: description.into(),
        input_schema: schema,
    }
}

fn read_tools() -> Vec<ToolDef> {
    vec![
        def(
            "list_dir",
            "List one directory of the project: subdirectories (ending in /) and files with sizes. Ignored files are hidden. Use it to explore an area the repo map shows only as a count.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Directory relative to the project root; \".\" for the root"}
            }}),
        ),
        def(
            "find_files",
            "Find files by glob, e.g. \"src/**/*.rs\" or \"**/*test*\". Call this when you know roughly what a file is called but not where it is.",
            json!({"type": "object", "properties": {
                "pattern": {"type": "string", "description": "Glob relative to the project root; * does not cross /, ** does"}
            }, "required": ["pattern"]}),
        ),
        def(
            "grep",
            "Search file contents with a regular expression. Returns path:line: text for up to 100 matches. Call this to find where a function, type, route, config key or error message is defined or used, before reading files.",
            json!({"type": "object", "properties": {
                "pattern": {"type": "string", "description": "Rust regex syntax, e.g. \"fn\\\\s+parse_\" or \"TODO\""},
                "path": {"type": "string", "description": "Only search under this directory or file"},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. \"**/*.py\""},
                "case_insensitive": {"type": "boolean"}
            }, "required": ["pattern"]}),
        ),
        def(
            "read_file",
            "Read a text file with line numbers, at most 400 lines per call. For large files, call outline or grep first and read only the lines you need.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "start_line": {"type": "integer", "description": "First line, 1-based (default 1)"},
                "end_line": {"type": "integer", "description": "Last line, inclusive"}
            }, "required": ["path"]}),
        ),
        def(
            "find_symbol",
            "Find where a function, type, class, method or constant is declared, by exact name. Returns path:line: declaration. Faster and more precise than grep when you know the name.",
            json!({"type": "object", "properties": {
                "name": {"type": "string", "description": "The exact identifier, e.g. \"parse_config\" or \"UserService\""}
            }, "required": ["name"]}),
        ),
        def(
            "references",
            "Find where a declared name is used across the project (whole-identifier matches, definitions excluded), up to 100 lines. Call this before changing a function or type, to see every caller you might need to update.",
            json!({"type": "object", "properties": {
                "name": {"type": "string", "description": "The exact identifier"}
            }, "required": ["name"]}),
        ),
        def(
            "related_files",
            "Show what a file depends on and which files depend on it, with the names that connect them, strongest links first. Call this to find a file's neighbours: its callers, the modules it uses, and the tests that exercise it.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"}
            }, "required": ["path"]}),
        ),
        def(
            "outline",
            "List the functions, types and other declarations in a file with their line numbers, without the bodies. Call this to understand a large file cheaply before reading parts of it.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"}
            }, "required": ["path"]}),
        ),
    ]
}

fn write_tools() -> Vec<ToolDef> {
    vec![
        def(
            "replace_in_file",
            "Edit an existing file by replacing an exact piece of text. old_text must match the file exactly (including indentation) and occur once, unless replace_all is true. Prefer this over write_file for changing existing files. Read the lines first so old_text is exact.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "old_text": {"type": "string", "description": "Exact text to replace; include enough surrounding lines to be unique"},
                "new_text": {"type": "string"},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
            }, "required": ["path", "old_text", "new_text"]}),
        ),
        def(
            "write_file",
            "Create a new file, or replace a small file entirely. Parent directories are created.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"},
                "content": {"type": "string", "description": "The complete file contents"}
            }, "required": ["path", "content"]}),
        ),
        def(
            "delete_file",
            "Delete a file.",
            json!({"type": "object", "properties": {
                "path": {"type": "string"}
            }, "required": ["path"]}),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, input: Value) -> ToolCall {
        ToolCall {
            id: "t".into(),
            name: name.into(),
            input,
        }
    }

    #[test]
    fn parses_calls_with_defaults() {
        assert_eq!(
            ToolRequest::parse(&call("list_dir", json!({}))).unwrap(),
            ToolRequest::ListDir { path: ".".into() }
        );
        let r = ToolRequest::parse(&call("grep", json!({"pattern": "x", "path": "src"}))).unwrap();
        assert_eq!(r.describe(), "grep \"x\" in src");
        assert!(ToolRequest::parse(&call("grep", json!({}))).is_err());
        assert!(ToolRequest::parse(&call("rm_rf", json!({}))).is_err());
    }

    #[test]
    fn read_only_kit_blocks_writes() {
        let w = ToolRequest::DeleteFile { path: "a".into() };
        assert!(!ToolKit::ReadOnly.allows(&w));
        assert!(ToolKit::ReadWrite.allows(&w));
        assert_eq!(ToolKit::ReadOnly.defs().len(), 8);
        assert_eq!(ToolKit::ReadWrite.defs().len(), 11);
    }
}
