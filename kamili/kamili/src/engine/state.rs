//! The build run as an explicit state machine. Pure: no I/O.
//!
//! The runner (`pipeline.rs`) performs the action each state implies, turns
//! the outcome into an [`Input`], and asks [`step`] for the next state. Every
//! state is saved before its action runs, so a run can resume after a crash,
//! an error or Ctrl+C.
//!
//! ```text
//! Planning ─► [AwaitingPlanApproval] ─► Working(n) ─► Testing(n) ─pass─► Documenting
//!                                          ▲              │fail
//!                                          └─ Diagnosing(n)◄┘ (n ≤ max_retries)
//! Documenting ─► [AwaitingWriteApproval] ─► WritingBack ─► Done
//! any ─► Failed | Cancelled   (both may keep a state to resume from)
//! ```
//!
//! Invariants, checked by the tests below:
//! - `WritingBack` is entered only from an approved `AwaitingWriteApproval`,
//!   or straight from `Documenting` when write-back approval is turned off.
//! - At most `max_retries + 1` Worker attempts.
//! - Terminal states (`Done`, `Failed`, `Cancelled`) never change.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum BuildState {
    Planning,
    AwaitingPlanApproval,
    Working {
        attempt: u32,
    },
    Testing {
        attempt: u32,
    },
    Diagnosing {
        attempt: u32,
    },
    Documenting,
    AwaitingWriteApproval,
    WritingBack,
    Done,
    Failed {
        reason: FailReason,
        /// Where `--resume` restarts, for failures worth retrying.
        resume_from: Option<Box<BuildState>>,
    },
    Cancelled {
        resume_from: Box<BuildState>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum FailReason {
    Rejected { what: String },
    TestsFailed { attempts: u32 },
    Error { message: String, exit_code: u8 },
}

/// What happened when the current state's action ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Input {
    /// The Planner finished; `spec_changed` if it proposed edits.
    Planned {
        spec_changed: bool,
    },
    Approved,
    Rejected,
    Worked,
    TestsPassed,
    TestsFailed,
    Diagnosed,
    Documented {
        nothing_to_write: bool,
    },
    Written,
    Failed {
        message: String,
        exit_code: u8,
        retryable: bool,
    },
    Cancel,
}

/// Settings that shape the path through the machine.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Policy {
    pub max_retries: u32,
    pub docs_only: bool,
    pub approve_spec_edits: bool,
    pub approve_write_back: bool,
}

impl BuildState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            BuildState::Done | BuildState::Failed { .. } | BuildState::Cancelled { .. }
        )
    }

    /// The state to continue from, if this run can be resumed.
    pub fn resume_point(&self) -> Option<BuildState> {
        match self {
            BuildState::Done => None,
            BuildState::Failed { resume_from, .. } => resume_from.as_deref().cloned(),
            BuildState::Cancelled { resume_from } => Some((**resume_from).clone()),
            other => Some(other.clone()),
        }
    }

    /// The pipeline step this state belongs to.
    pub fn step(&self) -> super::RunStep {
        use super::RunStep as R;
        match self {
            BuildState::Planning | BuildState::AwaitingPlanApproval => R::Plan,
            BuildState::Working { .. } => R::Work,
            BuildState::Testing { .. } | BuildState::Diagnosing { .. } => R::Test,
            BuildState::Documenting => R::Document,
            BuildState::AwaitingWriteApproval | BuildState::WritingBack => R::WriteBack,
            BuildState::Done => R::Done,
            BuildState::Failed { .. } | BuildState::Cancelled { .. } => R::Stopped,
        }
    }

    /// Whether the action of this state may be interrupted by Ctrl+C.
    pub fn cancellable(&self) -> bool {
        !matches!(self, BuildState::WritingBack)
    }
}

impl fmt::Display for BuildState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildState::Planning => write!(f, "planning"),
            BuildState::AwaitingPlanApproval => write!(f, "awaiting approval of spec edits"),
            BuildState::Working { attempt } => write!(f, "working (attempt {attempt})"),
            BuildState::Testing { attempt } => write!(f, "testing (attempt {attempt})"),
            BuildState::Diagnosing { attempt } => {
                write!(f, "diagnosing test failure (attempt {attempt})")
            }
            BuildState::Documenting => write!(f, "documenting"),
            BuildState::AwaitingWriteApproval => write!(f, "awaiting approval to write back"),
            BuildState::WritingBack => write!(f, "writing back"),
            BuildState::Done => write!(f, "done"),
            BuildState::Failed {
                reason,
                resume_from,
            } => {
                match reason {
                    FailReason::Rejected { what } => write!(f, "failed: rejected {what}")?,
                    FailReason::TestsFailed { attempts } => {
                        write!(f, "failed: tests failing after {attempts} attempt(s)")?
                    }
                    FailReason::Error { message, .. } => write!(f, "failed: {message}")?,
                }
                if let Some(at) = resume_from {
                    write!(f, " (resumable at {at})")?;
                }
                Ok(())
            }
            BuildState::Cancelled { resume_from } => {
                write!(f, "cancelled (resumable at {resume_from})")
            }
        }
    }
}

fn after_plan(p: &Policy) -> BuildState {
    if p.docs_only {
        BuildState::Documenting
    } else {
        BuildState::Working { attempt: 1 }
    }
}

/// The next state. Total: unexpected inputs become an internal failure rather
/// than a panic, and terminal states absorb everything.
pub(crate) fn step(state: &BuildState, input: Input, p: &Policy) -> BuildState {
    use BuildState as S;
    use Input as I;

    if state.is_terminal() {
        return state.clone();
    }
    match (state, input) {
        (_, I::Cancel) => S::Cancelled {
            resume_from: Box::new(state.clone()),
        },
        (
            _,
            I::Failed {
                message,
                exit_code,
                retryable,
            },
        ) => S::Failed {
            reason: FailReason::Error { message, exit_code },
            resume_from: retryable.then(|| Box::new(state.clone())),
        },

        (S::Planning, I::Planned { spec_changed }) => {
            if spec_changed && p.approve_spec_edits {
                S::AwaitingPlanApproval
            } else {
                after_plan(p)
            }
        }
        (S::AwaitingPlanApproval, I::Approved) => after_plan(p),
        (S::AwaitingPlanApproval, I::Rejected) => S::Failed {
            reason: FailReason::Rejected {
                what: "spec edits".into(),
            },
            resume_from: None,
        },

        (S::Working { attempt }, I::Worked) => S::Testing { attempt: *attempt },
        (S::Testing { .. }, I::TestsPassed) => S::Documenting,
        (S::Testing { attempt }, I::TestsFailed) => S::Diagnosing { attempt: *attempt },
        (S::Diagnosing { attempt }, I::Diagnosed) => {
            if *attempt > p.max_retries {
                S::Failed {
                    reason: FailReason::TestsFailed { attempts: *attempt },
                    resume_from: None,
                }
            } else {
                S::Working {
                    attempt: attempt + 1,
                }
            }
        }

        (S::Documenting, I::Documented { nothing_to_write }) => {
            if nothing_to_write {
                S::Done
            } else if p.approve_write_back {
                S::AwaitingWriteApproval
            } else {
                S::WritingBack
            }
        }
        (S::AwaitingWriteApproval, I::Approved) => S::WritingBack,
        (S::AwaitingWriteApproval, I::Rejected) => S::Failed {
            reason: FailReason::Rejected {
                what: "write-back".into(),
            },
            resume_from: None,
        },
        (S::WritingBack, I::Written) => S::Done,

        (state, input) => S::Failed {
            reason: FailReason::Error {
                message: format!("internal: unexpected {input:?} while {state}"),
                exit_code: 1,
            },
            resume_from: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use BuildState as S;

    const P: Policy = Policy {
        max_retries: 2,
        docs_only: false,
        approve_spec_edits: true,
        approve_write_back: true,
    };

    fn run(inputs: Vec<Input>, p: &Policy) -> Vec<BuildState> {
        let mut s = S::Planning;
        let mut seen = vec![s.clone()];
        for i in inputs {
            s = step(&s, i, p);
            seen.push(s.clone());
        }
        seen
    }

    #[test]
    fn happy_path_with_one_retry() {
        let seen = run(
            vec![
                Input::Planned { spec_changed: true },
                Input::Approved,
                Input::Worked,
                Input::TestsFailed,
                Input::Diagnosed,
                Input::Worked,
                Input::TestsPassed,
                Input::Documented {
                    nothing_to_write: false,
                },
                Input::Approved,
                Input::Written,
            ],
            &P,
        );
        assert_eq!(seen[3], S::Testing { attempt: 1 });
        assert_eq!(seen[5], S::Working { attempt: 2 });
        assert_eq!(seen[8], S::AwaitingWriteApproval);
        assert_eq!(seen.last(), Some(&S::Done));
    }

    #[test]
    fn retries_are_bounded() {
        let mut s = S::Working { attempt: 1 };
        let mut attempts = 1;
        loop {
            s = step(&s, Input::Worked, &P);
            s = step(&s, Input::TestsFailed, &P);
            s = step(&s, Input::Diagnosed, &P);
            match &s {
                S::Working { attempt } => attempts = *attempt,
                S::Failed {
                    reason,
                    resume_from,
                } => {
                    assert_eq!(*reason, FailReason::TestsFailed { attempts: 3 });
                    assert!(resume_from.is_none());
                    break;
                }
                other => panic!("unexpected {other}"),
            }
        }
        assert_eq!(attempts, P.max_retries + 1);
    }

    #[test]
    fn docs_only_and_no_gates() {
        let p = Policy {
            docs_only: true,
            approve_spec_edits: false,
            approve_write_back: false,
            ..P
        };
        let seen = run(
            vec![
                Input::Planned { spec_changed: true },
                Input::Documented {
                    nothing_to_write: false,
                },
                Input::Written,
            ],
            &p,
        );
        assert_eq!(seen, [S::Planning, S::Documenting, S::WritingBack, S::Done]);
    }

    #[test]
    fn failures_and_cancel_keep_resume_points() {
        let w = S::Working { attempt: 2 };
        let failed = step(
            &w,
            Input::Failed {
                message: "model down".into(),
                exit_code: 3,
                retryable: true,
            },
            &P,
        );
        assert_eq!(failed.resume_point(), Some(w.clone()));
        let cancelled = step(&w, Input::Cancel, &P);
        assert_eq!(cancelled.resume_point(), Some(w.clone()));
        let fatal = step(
            &w,
            Input::Failed {
                message: "conflict".into(),
                exit_code: 10,
                retryable: false,
            },
            &P,
        );
        assert_eq!(fatal.resume_point(), None);
        assert_eq!(S::Done.resume_point(), None);
        // Terminal states absorb every input.
        for t in [S::Done, failed, cancelled] {
            assert_eq!(step(&t, Input::Approved, &P), t);
        }
    }

    #[test]
    fn unexpected_input_is_an_internal_failure() {
        let s = step(&S::Planning, Input::Written, &P);
        assert!(matches!(
            s,
            S::Failed {
                resume_from: None,
                ..
            }
        ));
    }

    /// Explores every reachable state under every input and policy, and checks
    /// that write-back is only ever entered with approval (or with approval
    /// turned off).
    #[test]
    fn write_back_is_only_reachable_with_approval() {
        let inputs = || {
            vec![
                Input::Planned { spec_changed: true },
                Input::Planned {
                    spec_changed: false,
                },
                Input::Approved,
                Input::Rejected,
                Input::Worked,
                Input::TestsPassed,
                Input::TestsFailed,
                Input::Diagnosed,
                Input::Documented {
                    nothing_to_write: true,
                },
                Input::Documented {
                    nothing_to_write: false,
                },
                Input::Written,
                Input::Cancel,
                Input::Failed {
                    message: "x".into(),
                    exit_code: 3,
                    retryable: true,
                },
            ]
        };
        for approve_write_back in [true, false] {
            for docs_only in [true, false] {
                let p = Policy {
                    approve_write_back,
                    docs_only,
                    ..P
                };
                let mut frontier = vec![S::Planning];
                let mut seen: Vec<S> = Vec::new();
                while let Some(s) = frontier.pop() {
                    if seen.contains(&s) {
                        continue;
                    }
                    seen.push(s.clone());
                    for i in inputs() {
                        let next = step(&s, i.clone(), &p);
                        if next == S::WritingBack && s != S::WritingBack {
                            let ok = (s == S::AwaitingWriteApproval && i == Input::Approved)
                                || (!approve_write_back && s == S::Documenting);
                            assert!(ok, "{s} --{i:?}--> writing back");
                        }
                        if let S::Working { attempt } = next {
                            assert!(attempt <= p.max_retries + 1);
                        }
                        frontier.push(next);
                    }
                }
                assert!(seen.contains(&S::Done));
            }
        }
    }
}
