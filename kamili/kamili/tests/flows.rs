//! End-to-end flows through the public API only, with a scripted model.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kamili::{
    ApprovalRequest, BuildRequest, CancelToken, Channel, Decision, Engine, Event, FakeLlm,
    FakeReply, Question, SpecOutcome, SpecRequest,
};
use serde_json::json;

struct TestChannel {
    approve: bool,
    answers: Mutex<Vec<String>>,
    events: Mutex<Vec<Event>>,
    approvals: Mutex<Vec<ApprovalRequest>>,
}

impl TestChannel {
    fn new(approve: bool, answers: &[&str]) -> Self {
        Self {
            approve,
            answers: Mutex::new(answers.iter().rev().map(|s| s.to_string()).collect()),
            events: Mutex::new(Vec::new()),
            approvals: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Channel for TestChannel {
    fn event(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
    async fn ask(&self, _: &Question) -> Option<String> {
        self.answers.lock().unwrap().pop()
    }
    async fn approve(&self, request: &ApprovalRequest) -> Decision {
        self.approvals.lock().unwrap().push(request.clone());
        if self.approve {
            Decision::Approve
        } else {
            Decision::Reject
        }
    }
}

const SPEC: &str = "---\nstatus: approved\n---\n# Greet\n\n## Requirements\n- say hi\n\n## Change Log\n- created\n";

fn project(spec: &str, config: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("docs/features")).unwrap();
    fs::write(dir.path().join("docs/features/greet.md"), spec).unwrap();
    fs::write(dir.path().join(".kamili.toml"), config).unwrap();
    dir
}

fn engine(root: &Path, llm: Arc<FakeLlm>) -> Engine {
    Engine::new(kamili::load_config(root).unwrap(), llm).unwrap()
}

fn build_request(root: &Path) -> BuildRequest {
    BuildRequest {
        project_root: root.to_path_buf(),
        feature: "greet".into(),
        task: "create hello.txt".into(),
        ..Default::default()
    }
}

const CONFIG: &str =
    "[build]\ntest_command = [\"sh\", \"-c\", \"test -f hello.txt\"]\nmax_retries = 1\n";
const PLAN: &str = r#"{"summary":"needs hello.txt","section_edits":[{"section":"Requirements","content":"- say hi\n- hello.txt exists"}]}"#;
const WORK_OK: &str =
    r#"{"summary":"add hello.txt","files":[{"path":"hello.txt","content":"hi\n"}]}"#;
const WORK_BAD: &str = r#"{"summary":"oops","files":[{"path":"wrong.txt","content":"x"}]}"#;
const FEEDBACK: &str = r#"{"feedback":"hello.txt is missing"}"#;
const DOC: &str = r#"{"summary":"logged","section_edits":[{"section":"Change Log","content":"- added hello.txt\n- created"}]}"#;

#[tokio::test]
async fn build_happy_path_writes_code_and_spec() {
    let dir = project(SPEC, CONFIG);
    let llm = Arc::new(FakeLlm::new([PLAN, WORK_OK, DOC]));
    let ch = TestChannel::new(true, &[]);

    let out = engine(dir.path(), llm.clone())
        .build(build_request(dir.path()), &ch)
        .await
        .unwrap();

    assert_eq!(out.attempts, 1);
    assert_eq!(out.files_changed, ["hello.txt"]);
    assert!(out.spec_changed);
    assert_eq!(
        fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
        "hi\n"
    );
    let spec = fs::read_to_string(dir.path().join("docs/features/greet.md")).unwrap();
    assert!(spec.starts_with("---\nstatus: approved\n"));
    assert!(spec.contains("- hello.txt exists") && spec.contains("- added hello.txt"));
    assert_eq!(
        ch.approvals.lock().unwrap().len(),
        2,
        "spec-edit gate and write-back gate"
    );
    assert_eq!(llm.requests()[0].model, "claude-opus-5-5");
    assert_eq!(llm.requests()[1].model, "claude-sonnet-5-5");
}

#[tokio::test]
async fn build_retries_with_tester_feedback() {
    let dir = project(SPEC, CONFIG);
    let llm = Arc::new(FakeLlm::new([PLAN, WORK_BAD, FEEDBACK, WORK_OK, DOC]));
    let ch = TestChannel::new(true, &[]);

    let out = engine(dir.path(), llm.clone())
        .build(build_request(dir.path()), &ch)
        .await
        .unwrap();

    assert_eq!(out.attempts, 2);
    assert!(
        llm.requests()[3].messages[0]
            .content
            .contains("hello.txt is missing")
    );
    assert!(dir.path().join("hello.txt").exists());
}

#[tokio::test]
async fn failing_build_writes_nothing_and_saves_report() {
    let dir = project(SPEC, CONFIG);
    let llm = Arc::new(FakeLlm::new([PLAN, WORK_BAD, FEEDBACK, WORK_BAD, FEEDBACK]));
    let ch = TestChannel::new(true, &[]);

    let err = engine(dir.path(), llm)
        .build(build_request(dir.path()), &ch)
        .await
        .unwrap_err();

    assert_eq!(err.exit_code(), 7);
    assert!(!dir.path().join("wrong.txt").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("docs/features/greet.md")).unwrap(),
        SPEC
    );
    assert_eq!(
        fs::read_dir(dir.path().join(".kamili/runs"))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn draft_spec_stops_before_any_model_call() {
    let dir = project(&SPEC.replace("approved", "draft"), CONFIG);
    let llm = Arc::new(FakeLlm::new(Vec::<String>::new()));
    let ch = TestChannel::new(true, &[]);

    let err = engine(dir.path(), llm.clone())
        .build(build_request(dir.path()), &ch)
        .await
        .unwrap_err();

    assert_eq!(err.exit_code(), 6);
    assert!(llm.requests().is_empty());
}

#[tokio::test]
async fn planner_cannot_edit_documentor_sections() {
    let dir = project(SPEC, CONFIG);
    let plan =
        r#"{"summary":"sneaky","section_edits":[{"section":"Change Log","content":"rewritten"}]}"#;
    let llm = Arc::new(FakeLlm::new([plan]));
    let ch = TestChannel::new(true, &[]);

    let err = engine(dir.path(), llm)
        .build(build_request(dir.path()), &ch)
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 9);
}

#[tokio::test]
async fn worker_cannot_edit_specs() {
    let dir = project(SPEC, CONFIG);
    let work = r#"{"summary":"x","files":[{"path":"docs/features/greet.md","content":"hacked"}]}"#;
    let llm = Arc::new(FakeLlm::new([PLAN, work]));
    let ch = TestChannel::new(true, &[]);

    let err = engine(dir.path(), llm)
        .build(build_request(dir.path()), &ch)
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 4);
    assert_eq!(
        fs::read_to_string(dir.path().join("docs/features/greet.md")).unwrap(),
        SPEC
    );
}

#[tokio::test]
async fn rejected_write_back_leaves_tree_untouched() {
    let dir = project(SPEC, &format!("{CONFIG}approve_spec_edits = false\n"));
    let llm = Arc::new(FakeLlm::new([PLAN, WORK_OK, DOC]));
    let ch = TestChannel::new(false, &[]);

    let err = engine(dir.path(), llm)
        .build(build_request(dir.path()), &ch)
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 8);
    assert!(!dir.path().join("hello.txt").exists());
}

const DESIGN: &str = r####"{"summary":"simple","section_edits":[{"section":"Architecture","content":"### Approach\nPrint a greeting.\n"}]}"####;

fn spec_request(root: &Path) -> SpecRequest {
    SpecRequest {
        project_root: root.into(),
        feature: "greet".into(),
        resume: false,
    }
}

#[tokio::test]
async fn architect_may_not_edit_requirements() {
    let dir = tempfile::tempdir().unwrap();
    let llm = Arc::new(FakeLlm::new([
        r##"{"type":"draft","spec":"# Greet\n## Goal\nhi\n"}"##,
        r#"{"summary":"x","section_edits":[{"section":"Goal","content":"bye"}]}"#,
    ]));
    let err = engine(dir.path(), llm)
        .spec_session(spec_request(dir.path()), &TestChannel::new(true, &[]))
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 9);
    assert!(!dir.path().join("docs/features/greet.md").exists());
}

#[tokio::test]
async fn architect_can_be_turned_off() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join(".kamili.toml"),
        "[session]\narchitect = false\n",
    )
    .unwrap();
    let llm = Arc::new(FakeLlm::new([
        r##"{"type":"draft","spec":"# Greet\n## Goal\nhi\n"}"##,
    ]));
    let out = engine(dir.path(), llm.clone())
        .spec_session(spec_request(dir.path()), &TestChannel::new(true, &[]))
        .await
        .unwrap();
    assert!(matches!(out, SpecOutcome::Approved { .. }));
    assert_eq!(llm.remaining(), 0);
}

#[tokio::test]
async fn required_skills_reach_their_agents_only() {
    let dir = project(
        SPEC,
        &format!("{CONFIG}\n[skills.required]\nworker = [\"acme/skills@house-style\"]\n"),
    );
    let skill = dir.path().join(".agents/skills/house-style");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: house-style\ndescription: Our conventions\n---\nAlways end files with a newline.\n",
    )
    .unwrap();
    let llm = Arc::new(FakeLlm::new([PLAN, WORK_OK, DOC]));
    engine(dir.path(), llm.clone())
        .build(build_request(dir.path()), &TestChannel::new(true, &[]))
        .await
        .unwrap();

    let reqs = llm.requests();
    assert!(
        reqs[1].system.contains("Always end files with a newline."),
        "Worker"
    );
    assert!(!reqs[0].system.contains("house-style"), "Planner");
    assert!(!reqs[2].system.contains("house-style"), "Documentor");
}

#[tokio::test]
async fn missing_required_skill_stops_before_any_model_call() {
    let dir = project(
        SPEC,
        &format!("{CONFIG}\n[skills.required]\nall = [\"acme/skills@missing\"]\n"),
    );
    let llm = Arc::new(FakeLlm::new([PLAN, WORK_OK, DOC]));
    let err = engine(dir.path(), llm.clone())
        .build(build_request(dir.path()), &TestChannel::new(true, &[]))
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(
        err.to_string()
            .contains("npx skills add acme/skills --skill missing -y")
    );
    assert_eq!(llm.requests().len(), 0);
    assert!(!dir.path().join("hello.txt").exists());
}

#[tokio::test]
async fn spec_session_interviews_then_writes_approved_spec() {
    let dir = tempfile::tempdir().unwrap();
    let llm = Arc::new(FakeLlm::new([
        r#"{"type":"question","text":"Who is greeted?"}"#,
        r##"{"type":"draft","spec":"# Greet\n\n## Goal\nGreet Alice.\n\n## Open Questions\n- none\n"}"##,
        DESIGN,
    ]));
    let ch = TestChannel::new(true, &["Alice"]);
    let req = SpecRequest {
        project_root: dir.path().into(),
        feature: "greet".into(),
        resume: false,
    };

    let out = engine(dir.path(), llm.clone())
        .spec_session(req, &ch)
        .await
        .unwrap();

    let path = dir.path().join("docs/features/greet.md");
    assert_eq!(out, SpecOutcome::Approved { path: path.clone() });
    let text = fs::read_to_string(path).unwrap();
    assert!(text.starts_with("---\nstatus: approved\n---\n# Greet"));
    assert!(text.contains("## Architecture\n### Approach\nPrint a greeting."));
    let architect = &llm.requests()[2];
    assert_eq!(architect.model, "claude-opus-5-5");
    assert!(architect.system.starts_with("You are the Architect"));
    assert!(architect.messages[0].content.contains("Greet Alice."));
    let second = &llm.requests()[1].messages;
    assert_eq!(second.len(), 3);
    assert_eq!(second[2].content, "Alice");
    assert!(dir.path().join(".kamili/sessions/greet.json").exists());
}

#[tokio::test]
async fn spec_session_without_answer_is_abandoned_and_resumable() {
    let dir = tempfile::tempdir().unwrap();
    let llm = Arc::new(FakeLlm::new([r#"{"type":"question","text":"Who?"}"#]));
    let ch = TestChannel::new(true, &[]);
    let req = SpecRequest {
        project_root: dir.path().into(),
        feature: "greet".into(),
        resume: false,
    };

    let out = engine(dir.path(), llm)
        .spec_session(req, &ch)
        .await
        .unwrap();
    assert_eq!(out, SpecOutcome::Abandoned);
    assert!(!dir.path().join("docs/features/greet.md").exists());

    // Resume: the pending question is re-asked, so the model sees a transcript
    // ending on the user's answer. The channel rejects the draft.
    let llm = Arc::new(FakeLlm::new([
        r##"{"type":"draft","spec":"# Greet\n## Goal\nhi\n"}"##,
        DESIGN,
    ]));
    let ch = TestChannel::new(false, &["Bob"]);
    let req = SpecRequest {
        project_root: dir.path().into(),
        feature: "greet".into(),
        resume: true,
    };

    let out = engine(dir.path(), llm.clone())
        .spec_session(req, &ch)
        .await
        .unwrap();

    assert_eq!(out, SpecOutcome::Rejected);
    let sent = &llm.requests()[0].messages;
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1].content, "Who?");
    assert_eq!(sent[2].content, "Bob");
    assert!(!dir.path().join("docs/features/greet.md").exists());
}

// ---------------------------------------------------------------------------
// Tools, run state, resume and cancel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn worker_edits_existing_code_with_tools_guided_by_the_plan() {
    let dir = project(
        SPEC,
        "[build]\ntest_command = [\"grep\", \"-q\", \"Hello\", \"src/greet.txt\"]\n",
    );
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/greet.txt"), "header\nhi\nfooter\n").unwrap();
    let llm = Arc::new(FakeLlm::new([
        FakeReply::tool("grep", json!({"pattern": "^hi$"})),
        r#"{"summary":"change the greeting","context_brief":"The greeting is line 2 of src/greet.txt."}"#.into(),
        FakeReply::tool("read_file", json!({"path": "src/greet.txt", "start_line": 2, "end_line": 2})),
        FakeReply::tool("replace_in_file", json!({"path": "src/greet.txt", "old_text": "hi\n", "new_text": "Hello\n"})),
        r#"{"summary":"greeting is now Hello"}"#.into(),
        DOC.into(),
    ]));
    let ch = TestChannel::new(true, &[]);

    let out = engine(dir.path(), llm.clone())
        .build(build_request(dir.path()), &ch)
        .await
        .unwrap();

    assert_eq!(out.files_changed, ["src/greet.txt"]);
    assert_eq!(
        fs::read_to_string(dir.path().join("src/greet.txt")).unwrap(),
        "header\nHello\nfooter\n"
    );
    let reqs = llm.chat_requests();
    // The Planner's grep found the line; its brief and the repo map reach the Worker.
    assert!(reqs[1].messages[2].text().contains("src/greet.txt:2: hi"));
    let worker_prompt = reqs[2].messages[0].text();
    assert!(worker_prompt.contains("<context_brief>\nThe greeting is line 2"));
    assert!(worker_prompt.contains("<repo_map>") && worker_prompt.contains("greet.txt"));
    assert_eq!(reqs[2].tools.len(), 11, "Worker has read and edit tools");
    assert_eq!(reqs[0].tools.len(), 8, "Planner has read tools only");

    let events = ch.events.lock().unwrap();
    let tool_uses: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolUse { description, .. } => Some(description.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_uses,
        [
            "grep \"^hi$\"",
            "read src/greet.txt:2-2",
            "edit src/greet.txt"
        ]
    );
    let states: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::StateChanged { state, .. } => Some(state.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(states.first().map(String::as_str), Some("planning"));
    assert_eq!(states.last().map(String::as_str), Some("done"));

    // The run record and journal are on disk; usage is counted.
    let report = fs::read_to_string(out.report_path.unwrap()).unwrap();
    let report: serde_json::Value = serde_json::from_str(&report).unwrap();
    assert_eq!(report["state"]["state"], "done");
    assert_eq!(report["usage"]["Worker"]["tool_calls"], 2);
    let journal = fs::read_to_string(
        dir.path()
            .join(".kamili/runs")
            .join(&out.run_id)
            .join("journal.jsonl"),
    )
    .unwrap();
    assert_eq!(journal.lines().count(), 6, "{journal}");
}

#[tokio::test]
async fn failed_run_resumes_without_repeating_finished_agents() {
    let dir = project(SPEC, CONFIG);
    // The model "goes down" before the Documentor.
    let llm = Arc::new(FakeLlm::new([PLAN, WORK_OK]));
    let ch = TestChannel::new(true, &[]);
    let err = engine(dir.path(), llm)
        .build(build_request(dir.path()), &ch)
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 3);
    assert!(
        !dir.path().join("hello.txt").exists(),
        "nothing written yet"
    );

    let runs = kamili::list_runs(dir.path());
    assert_eq!(runs.len(), 1);
    assert!(runs[0].resumable, "{}", runs[0].state);
    assert!(
        runs[0].state.contains("resumable at documenting"),
        "{}",
        runs[0].state
    );

    // Resume: only the Documentor is called; the earlier work is reused.
    let llm = Arc::new(FakeLlm::new([DOC]));
    let req = BuildRequest {
        project_root: dir.path().into(),
        resume: Some(runs[0].id.clone()),
        ..Default::default()
    };
    let out = engine(dir.path(), llm.clone())
        .build(req, &ch)
        .await
        .unwrap();
    assert_eq!(llm.requests().len(), 1);
    assert!(
        llm.requests()[0]
            .system
            .starts_with("You are the Documentor")
    );
    assert_eq!(out.files_changed, ["hello.txt"]);
    assert_eq!(
        fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
        "hi\n"
    );
    assert!(!kamili::list_runs(dir.path())[0].resumable);
}

#[tokio::test]
async fn cancelled_run_resumes_with_last() {
    let dir = project(SPEC, CONFIG);
    let token = CancelToken::new();
    token.cancel();
    let llm = Arc::new(FakeLlm::new(Vec::<String>::new()));
    let ch = TestChannel::new(true, &[]);
    let req = BuildRequest {
        cancel: Some(token),
        ..build_request(dir.path())
    };
    let err = engine(dir.path(), llm.clone())
        .build(req, &ch)
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 130);
    assert!(llm.requests().is_empty());

    let llm = Arc::new(FakeLlm::new([PLAN, WORK_OK, DOC]));
    let req = BuildRequest {
        project_root: dir.path().into(),
        resume: Some("last".into()),
        ..Default::default()
    };
    engine(dir.path(), llm).build(req, &ch).await.unwrap();
    assert!(dir.path().join("hello.txt").exists());
}

#[tokio::test]
async fn finished_runs_are_not_resumable_and_can_be_cleaned() {
    let dir = project(SPEC, CONFIG);
    let llm = Arc::new(FakeLlm::new([PLAN]));
    let err = engine(dir.path(), llm)
        .build(build_request(dir.path()), &TestChannel::new(false, &[]))
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 8, "rejected");
    let id = kamili::list_runs(dir.path())[0].id.clone();

    let req = BuildRequest {
        project_root: dir.path().into(),
        resume: Some(id),
        ..Default::default()
    };
    let err = engine(dir.path(), Arc::new(FakeLlm::new(Vec::<String>::new())))
        .build(req, &TestChannel::new(true, &[]))
        .await
        .unwrap_err();
    assert_eq!(err.exit_code(), 2);
    assert!(err.to_string().contains("can be resumed"));

    assert_eq!(kamili::clean_runs(dir.path(), true).unwrap(), 1);
    assert!(kamili::list_runs(dir.path()).is_empty());

    // An unfinished run is kept by clean, but can be discarded.
    let token = CancelToken::new();
    token.cancel();
    let req = BuildRequest {
        cancel: Some(token),
        ..build_request(dir.path())
    };
    let _ = engine(dir.path(), Arc::new(FakeLlm::new(Vec::<String>::new())))
        .build(req, &TestChannel::new(true, &[]))
        .await;
    assert_eq!(kamili::clean_runs(dir.path(), true).unwrap(), 0);
    kamili::discard_run(dir.path(), "last").unwrap();
    assert!(kamili::list_runs(dir.path()).is_empty());
}

#[tokio::test]
async fn code_graph_guides_agents_and_shows_impact() {
    let dir = project(
        SPEC,
        "[build]\ntest_command = [\"grep\", \"-q\", \"Hello\", \"src/greeting.rs\"]\n",
    );
    let w = |p: &str, s: &str| fs::write(dir.path().join(p), s).unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    w(
        "src/greeting.rs",
        "pub fn greeting_text() -> &'static str {\n    \"hi\"\n}\n",
    );
    w(
        "src/main.rs",
        "fn main() {\n    println!(\"{}\", greeting_text());\n}\n",
    );
    w("src/other.rs", "pub fn unrelated_helper() {}\n");
    let llm = Arc::new(FakeLlm::new([
        FakeReply::tool("references", json!({"name": "greeting_text"})),
        r#"{"summary":"change the text","context_brief":"greeting_text in src/greeting.rs"}"#
            .into(),
        FakeReply::tool("find_symbol", json!({"name": "greeting_text"})),
        FakeReply::tool(
            "replace_in_file",
            json!({"path": "src/greeting.rs", "old_text": "\"hi\"", "new_text": "\"Hello\""}),
        ),
        r#"{"summary":"greeting is Hello"}"#.into(),
        DOC.into(),
    ]));
    let ch = TestChannel::new(true, &[]);
    let out = engine(dir.path(), llm.clone())
        .build(
            BuildRequest {
                task: "make greeting_text say Hello".into(),
                ..build_request(dir.path())
            },
            &ch,
        )
        .await
        .unwrap();
    assert_eq!(out.files_changed, ["src/greeting.rs"]);

    let reqs = llm.chat_requests();
    // The Planner's map ranks the file the task names first.
    let planner_prompt = reqs[0].messages[0].text();
    let key = planner_prompt.split("<key_symbols>").nth(1).unwrap();
    assert!(
        key.contains("src/greeting.rs\n     1  pub fn greeting_text()"),
        "{key}"
    );
    // Graph tools answered from the code.
    assert!(reqs[1].messages[2].text().contains("src/main.rs:2:"));
    assert!(
        reqs[3].messages[2]
            .text()
            .contains("src/greeting.rs:1: pub fn greeting_text()")
    );
    // The write-back approval names the caller that wasn't changed.
    let approvals = ch.approvals.lock().unwrap();
    let write_back = approvals.last().unwrap();
    assert!(
        write_back
            .summary
            .contains("These files use the changed code")
            && write_back
                .summary
                .contains("src/main.rs (uses greeting_text)")
            && !write_back.summary.contains("other.rs"),
        "{}",
        write_back.summary
    );
}
