//! Find, parse, validate, diff and write feature specs. Leaf module.
//!
//! This module is the rule-keeper: status checks, section ownership and diffs
//! live here so agents cannot bypass them.
//!
//! # File format
//!
//! ```markdown
//! ---
//! status: draft | approved
//! ---
//! # Feature title
//! Optional preamble.
//!
//! ## Section heading
//! Section body …
//! ```
//!
//! Front matter is `key: value` lines; only `status` is required. Sections are
//! split on `## ` headings outside code fences. Unknown front-matter keys are
//! preserved.

use std::fmt;
use std::path::{Path, PathBuf};

/// Sections owned by the Documentor. The Planner may not touch them.
pub(crate) const DOC_SECTIONS: &[&str] = &["Change Log", "API Contracts", "Ops Notes"];
/// Sections owned by the Architect. The Planner may not touch them.
pub(crate) const ARCH_SECTIONS: &[&str] = &["Architecture"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Draft,
    Approved,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Status::Draft => "draft",
            Status::Approved => "approved",
        })
    }
}

/// Who is proposing an edit. Decides which sections may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Editor {
    Architect,
    Planner,
    Documentor,
}

impl Editor {
    fn may_edit(self, heading: &str) -> bool {
        let owned_by = |list: &[&str]| list.iter().any(|s| s.eq_ignore_ascii_case(heading));
        let doc_owned = owned_by(DOC_SECTIONS);
        let arch_owned = owned_by(ARCH_SECTIONS);
        match self {
            Editor::Architect => arch_owned,
            Editor::Planner => !doc_owned && !arch_owned,
            Editor::Documentor => doc_owned,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Section {
    pub heading: String,
    /// Raw body text including its trailing newlines.
    pub body: String,
}

/// A proposed replacement of one section's body (creates it if missing).
#[derive(Debug, Clone)]
pub(crate) struct SectionEdit {
    pub section: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Spec {
    pub path: PathBuf,
    pub status: Status,
    /// Front matter other than `status`, in original order.
    extra_front_matter: Vec<(String, String)>,
    preamble: String,
    pub sections: Vec<Section>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SpecError {
    #[error("no spec for feature `{feature}` at {path}")]
    NotFound { feature: String, path: PathBuf },
    #[error("invalid feature name `{0}`: use letters, digits, `-` or `_`")]
    InvalidName(String),
    #[error("cannot access {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {reason}")]
    Malformed { path: PathBuf, reason: String },
    #[error("{editor:?} may not edit section `{section}`")]
    Forbidden { editor: Editor, section: String },
    #[error("invalid section heading `{0}`")]
    InvalidHeading(String),
    #[error("{0} changed on disk during the run; not overwriting")]
    Conflict(PathBuf),
}

/// Path of a feature's spec file.
pub(crate) fn locate(specs_dir: &Path, feature: &str) -> Result<PathBuf, SpecError> {
    let valid = !feature.is_empty()
        && feature
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        return Err(SpecError::InvalidName(feature.to_string()));
    }
    Ok(specs_dir.join(format!("{feature}.md")))
}

/// Loads a spec. Returns the parsed spec and the exact text read from disk.
pub(crate) fn load(specs_dir: &Path, feature: &str) -> Result<(Spec, String), SpecError> {
    load_optional(specs_dir, feature)?.ok_or_else(|| SpecError::NotFound {
        feature: feature.to_string(),
        path: specs_dir.join(format!("{feature}.md")),
    })
}

/// Like [`load`], but `Ok(None)` when the file does not exist.
pub(crate) fn load_optional(
    specs_dir: &Path,
    feature: &str,
) -> Result<Option<(Spec, String)>, SpecError> {
    let path = locate(specs_dir, feature)?;
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some((parse(&path, &text)?, text))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SpecError::Io { path, source }),
    }
}

/// Parses a full spec file (with front matter).
pub(crate) fn parse(path: &Path, text: &str) -> Result<Spec, SpecError> {
    let malformed = |reason: &str| SpecError::Malformed {
        path: path.to_path_buf(),
        reason: reason.into(),
    };

    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| malformed("missing front matter (file must start with `---`)"))?;
    let end = rest
        .find("\n---\n")
        .map(|i| (i, i + "\n---\n".len()))
        .or_else(|| rest.strip_suffix("\n---").map(|r| (r.len(), rest.len())))
        .ok_or_else(|| malformed("unterminated front matter"))?;
    let (front, body) = (&rest[..end.0], &rest[end.1..]);

    let mut status = None;
    let mut extra = Vec::new();
    for line in front.lines().filter(|l| !l.trim().is_empty()) {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| malformed(&format!("bad front matter line `{line}`")))?;
        let (key, value) = (key.trim(), value.trim());
        if key == "status" {
            status = Some(match value {
                "draft" => Status::Draft,
                "approved" => Status::Approved,
                other => return Err(malformed(&format!("unknown status `{other}`"))),
            });
        } else {
            extra.push((key.to_string(), value.to_string()));
        }
    }
    let status = status.ok_or_else(|| malformed("front matter has no `status`"))?;

    let (preamble, sections) = split_sections(body);
    Ok(Spec {
        path: path.to_path_buf(),
        status,
        extra_front_matter: extra,
        preamble,
        sections,
    })
}

/// Builds a draft spec from markdown without front matter (e.g. an Analyst draft).
/// Front matter, if present, is discarded: status is always set by the engine.
pub(crate) fn from_draft(path: &Path, markdown: &str) -> Spec {
    let body = markdown
        .strip_prefix("---\n")
        .and_then(|r| r.find("\n---\n").map(|i| &r[i + 5..]))
        .unwrap_or(markdown);
    let (preamble, sections) = split_sections(body);
    Spec {
        path: path.to_path_buf(),
        status: Status::Draft,
        extra_front_matter: Vec::new(),
        preamble,
        sections,
    }
}

fn split_sections(body: &str) -> (String, Vec<Section>) {
    let mut preamble = String::new();
    let mut sections: Vec<Section> = Vec::new();
    let mut in_fence = false;
    for line in body.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        if !in_fence && let Some(heading) = line.strip_prefix("## ") {
            sections.push(Section {
                heading: heading.trim().to_string(),
                body: String::new(),
            });
            continue;
        }
        match sections.last_mut() {
            Some(s) => s.body.push_str(line),
            None => preamble.push_str(line),
        }
    }
    (preamble, sections)
}

impl Spec {
    /// Serialises back to the file format. Round-trips unedited files exactly
    /// (modulo front-matter whitespace).
    pub fn render(&self) -> String {
        let mut out = format!("---\nstatus: {}\n", self.status);
        for (k, v) in &self.extra_front_matter {
            out.push_str(&format!("{k}: {v}\n"));
        }
        out.push_str("---\n");
        out.push_str(&self.preamble);
        for s in &self.sections {
            out.push_str(&format!("## {}\n", s.heading));
            out.push_str(&s.body);
        }
        out
    }

    /// Returns a copy with `edits` applied, after checking `editor` owns every
    /// touched section. All-or-nothing.
    pub fn with_edits(&self, editor: Editor, edits: &[SectionEdit]) -> Result<Spec, SpecError> {
        let mut next = self.clone();
        for edit in edits {
            let heading = edit.section.trim();
            if heading.is_empty() || heading.contains('\n') || heading.starts_with('#') {
                return Err(SpecError::InvalidHeading(edit.section.clone()));
            }
            if !editor.may_edit(heading) {
                return Err(SpecError::Forbidden {
                    editor,
                    section: heading.to_string(),
                });
            }
            let body = format!("{}\n\n", edit.content.trim_end());
            match next
                .sections
                .iter_mut()
                .find(|s| s.heading.eq_ignore_ascii_case(heading))
            {
                Some(s) => s.body = body,
                None => {
                    if let Some(last) = next.sections.last_mut()
                        && !last.body.ends_with("\n\n")
                    {
                        last.body.push('\n');
                    }
                    next.sections.push(Section {
                        heading: heading.to_string(),
                        body,
                    });
                }
            }
        }
        Ok(next)
    }

    /// Writes the spec, refusing if the file no longer matches `expected_on_disk`
    /// (`None` = the file must not exist yet).
    pub fn write_if_unchanged(&self, expected_on_disk: Option<&str>) -> Result<(), SpecError> {
        self.check_unchanged(expected_on_disk)?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|source| SpecError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(&self.path, self.render()).map_err(|source| SpecError::Io {
            path: self.path.clone(),
            source,
        })
    }

    pub fn check_unchanged(&self, expected_on_disk: Option<&str>) -> Result<(), SpecError> {
        let current = match std::fs::read_to_string(&self.path) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(SpecError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        if current.as_deref() != expected_on_disk {
            return Err(SpecError::Conflict(self.path.clone()));
        }
        Ok(())
    }
}

/// Unified diff of two spec texts; empty when identical.
pub(crate) fn diff(old: &str, new: &str, label: &str) -> String {
    if old == new {
        return String::new();
    }
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{label}"), &format!("b/{label}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nstatus: approved\nowner: team-a\n---\n# Greeting\n\nIntro.\n\n## Requirements\n- say hi\n\n```md\n## not a section\n```\n\n## Change Log\n- created\n";

    fn sample() -> Spec {
        parse(Path::new("greet.md"), SAMPLE).unwrap()
    }

    #[test]
    fn round_trips_exactly() {
        let spec = sample();
        assert_eq!(spec.status, Status::Approved);
        assert_eq!(spec.sections.len(), 2, "fenced heading must not split");
        assert_eq!(spec.render(), SAMPLE);
    }

    #[test]
    fn rejects_missing_status() {
        assert!(parse(Path::new("x.md"), "---\nowner: a\n---\nbody\n").is_err());
    }

    #[test]
    fn planner_cannot_touch_doc_sections() {
        let edit = SectionEdit {
            section: "Change Log".into(),
            content: "x".into(),
        };
        assert!(matches!(
            sample().with_edits(Editor::Planner, &[edit]),
            Err(SpecError::Forbidden { .. })
        ));
    }

    #[test]
    fn only_the_architect_touches_architecture() {
        let edit = |section: &str| SectionEdit {
            section: section.into(),
            content: "x".into(),
        };
        assert!(
            sample()
                .with_edits(Editor::Architect, &[edit("Architecture")])
                .is_ok()
        );
        assert!(
            sample()
                .with_edits(Editor::Architect, &[edit("Requirements")])
                .is_err()
        );
        assert!(
            sample()
                .with_edits(Editor::Planner, &[edit("architecture")])
                .is_err()
        );
    }

    #[test]
    fn documentor_only_touches_doc_sections() {
        let ok = SectionEdit {
            section: "change log".into(),
            content: "- updated".into(),
        };
        let spec = sample().with_edits(Editor::Documentor, &[ok]).unwrap();
        assert!(spec.render().contains("## Change Log\n- updated\n"));

        let bad = SectionEdit {
            section: "Requirements".into(),
            content: "x".into(),
        };
        assert!(sample().with_edits(Editor::Documentor, &[bad]).is_err());
    }

    #[test]
    fn edits_can_add_sections_and_diff_shows_them() {
        let edit = SectionEdit {
            section: "Edge Cases".into(),
            content: "- empty name".into(),
        };
        let spec = sample().with_edits(Editor::Planner, &[edit]).unwrap();
        let d = diff(SAMPLE, &spec.render(), "greet.md");
        assert!(d.contains("+## Edge Cases"));
        assert!(diff(SAMPLE, SAMPLE, "greet.md").is_empty());
    }

    #[test]
    fn feature_names_cannot_escape_specs_dir() {
        assert!(locate(Path::new("specs"), "../etc/passwd").is_err());
        assert!(locate(Path::new("specs"), "user-auth_v2").is_ok());
    }

    #[test]
    fn draft_ignores_model_supplied_front_matter() {
        let spec = from_draft(
            Path::new("x.md"),
            "---\nstatus: approved\n---\n# X\n## Goal\nship\n",
        );
        assert_eq!(spec.status, Status::Draft);
        assert_eq!(spec.sections[0].heading, "Goal");
    }
}
