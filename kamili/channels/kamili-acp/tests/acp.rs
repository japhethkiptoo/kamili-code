//! Drives the ACP server like an editor would, over in-memory pipes, with a
//! scripted model behind the engine.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use kamili::{Engine, Error, FakeLlm};
use kamili_acp::{EngineFactory, serve_io};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines};

struct Factory(Mutex<Option<Arc<FakeLlm>>>);

#[async_trait]
impl EngineFactory for Factory {
    async fn engine(&self, root: &Path) -> Result<Engine, Error> {
        let llm = self
            .0
            .lock()
            .unwrap()
            .clone()
            .expect("one session per test");
        Engine::new(kamili::load_config(root)?, llm)
    }
}

struct Client {
    tx: DuplexStream,
    rx: Lines<BufReader<DuplexStream>>,
    next_id: i64,
    /// Every notification received, in order.
    updates: Vec<Value>,
}

impl Client {
    fn start(llm: Arc<FakeLlm>) -> Self {
        let (client_w, agent_r) = tokio::io::duplex(1 << 20);
        let (agent_w, client_r) = tokio::io::duplex(1 << 20);
        let factory = Arc::new(Factory(Mutex::new(Some(llm))));
        tokio::spawn(serve_io(factory, BufReader::new(agent_r), agent_w));
        Self {
            tx: client_w,
            rx: BufReader::new(client_r).lines(),
            next_id: 100,
            updates: Vec::new(),
        }
    }

    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_vec(&message).unwrap();
        line.push(b'\n');
        self.tx.write_all(&line).await.unwrap();
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(std::time::Duration::from_secs(10), self.rx.next_line())
            .await
            .expect("agent answered in time")
            .unwrap()
            .expect("agent still connected");
        let v: Value = serde_json::from_str(&line).unwrap();
        assert!(!line.contains('\n'));
        v
    }

    /// Sends a request and returns its result. Permission requests that
    /// arrive meanwhile are answered by `decide` (option id, or `None` to
    /// leave them unanswered and return the request instead).
    async fn call(
        &mut self,
        method: &str,
        params: Value,
        mut decide: impl FnMut(&Value) -> Option<&'static str>,
    ) -> Result<Value, Value> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let m = self.next().await;
            if m["id"] == id && m.get("method").is_none() {
                return match m.get("error") {
                    Some(e) => Err(e.clone()),
                    None => Ok(m["result"].clone()),
                };
            }
            if m["method"] == "session/request_permission" {
                match decide(&m["params"]) {
                    Some(option) => {
                        self.send(json!({"jsonrpc": "2.0", "id": m["id"], "result":
                            {"outcome": {"outcome": "selected", "optionId": option}}}))
                            .await
                    }
                    None => return Err(m),
                }
            } else {
                self.updates.push(m);
            }
        }
    }

    async fn new_session(&mut self, cwd: &Path) -> String {
        let r = self
            .call(
                "session/new",
                json!({"cwd": cwd.to_str().unwrap(), "mcpServers": []}),
                |_| None,
            )
            .await
            .unwrap();
        r["sessionId"].as_str().unwrap().to_string()
    }

    async fn prompt(&mut self, sid: &str, text: &str) -> Value {
        self.call(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
            |_| Some("approve"),
        )
        .await
        .unwrap()
    }

    fn texts(&self, kind: &str) -> String {
        self.updates
            .iter()
            .filter(|u| u["params"]["update"]["sessionUpdate"] == kind)
            .filter_map(|u| u["params"]["update"]["content"]["text"].as_str())
            .collect()
    }

    fn of_kind(&self, kind: &str) -> Vec<&Value> {
        self.updates
            .iter()
            .map(|u| &u["params"]["update"])
            .filter(|u| u["sessionUpdate"] == kind)
            .collect()
    }
}

const SPEC: &str = "---\nstatus: approved\n---\n# Greet\n\n## Requirements\n- say hi\n\n## Change Log\n- created\n";
const CONFIG: &str =
    "[build]\ntest_command = [\"sh\", \"-c\", \"test -f hello.txt\"]\nmax_retries = 1\n";
const PLAN: &str = r#"{"summary":"needs hello.txt","section_edits":[{"section":"Requirements","content":"- say hi\n- hello.txt exists"}]}"#;
const WORK_OK: &str =
    r#"{"summary":"add hello.txt","files":[{"path":"hello.txt","content":"hi\n"}]}"#;
const DOC: &str = r#"{"summary":"logged","section_edits":[{"section":"Change Log","content":"- added hello.txt\n- created"}]}"#;

fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("docs/features")).unwrap();
    fs::write(dir.path().join("docs/features/greet.md"), SPEC).unwrap();
    fs::write(dir.path().join(".kamili.toml"), CONFIG).unwrap();
    dir
}

#[tokio::test]
async fn initialize_and_commands() {
    let dir = project();
    let mut c = Client::start(Arc::new(FakeLlm::new(Vec::<String>::new())));
    let init = c
        .call(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
            |_| None,
        )
        .await
        .unwrap();
    assert_eq!(init["protocolVersion"], 1);
    assert_eq!(init["agentCapabilities"]["loadSession"], false);
    assert_eq!(init["agentInfo"]["name"], "kamili");

    // Relative cwd is rejected, unknown methods error.
    let bad = c
        .call(
            "session/new",
            json!({"cwd": "rel", "mcpServers": []}),
            |_| None,
        )
        .await;
    assert_eq!(bad.unwrap_err()["code"], -32602);
    let unknown = c.call("nope", json!({}), |_| None).await;
    assert_eq!(unknown.unwrap_err()["code"], -32601);

    let sid = c.new_session(dir.path()).await;
    let r = c.prompt(&sid, "/help").await;
    assert_eq!(r["stopReason"], "end_turn");
    let commands = c.of_kind("available_commands_update");
    let names: Vec<&str> = commands[0]["availableCommands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"spec") && names.contains(&"build"));
    assert!(
        c.texts("agent_message_chunk")
            .contains("/build <feature> <task>")
    );
}

#[tokio::test]
async fn build_reports_progress_and_asks_with_diffs() {
    let dir = project();
    let mut c = Client::start(Arc::new(FakeLlm::new([PLAN, WORK_OK, DOC])));
    let sid = c.new_session(dir.path()).await;

    let mut asked: Vec<Value> = Vec::new();
    let r = c
        .call(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": "/build greet create hello.txt"}]}),
            |p| {
                asked.push(p.clone());
                Some("approve")
            },
        )
        .await
        .unwrap();
    assert_eq!(r["stopReason"], "end_turn");
    assert_eq!(
        fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
        "hi\n"
    );

    // Two gates: the Planner's spec edits, then write-back.
    assert_eq!(asked.len(), 2);
    assert_eq!(asked[0]["options"][0]["kind"], "allow_once");
    // The write-back tool call carries real diffs (absolute paths).
    let edits: Vec<&Value> = c
        .of_kind("tool_call")
        .into_iter()
        .filter(|t| t["kind"] == "edit")
        .collect();
    let write_back = edits.last().unwrap();
    let diffs: Vec<&Value> = write_back["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["type"] == "diff")
        .collect();
    let hello = diffs
        .iter()
        .find(|d| d["path"].as_str().unwrap().ends_with("/hello.txt"))
        .expect("hello.txt diff");
    assert!(Path::new(hello["path"].as_str().unwrap()).is_absolute());
    assert_eq!(hello["oldText"], Value::Null);
    assert_eq!(hello["newText"], "hi\n");
    assert!(
        diffs
            .iter()
            .any(|d| d["path"].as_str().unwrap().ends_with("greet.md"))
    );

    // A plan that ends fully completed, a test tool call, and a final report.
    let plans = c.of_kind("plan");
    let last = plans.last().unwrap()["entries"].as_array().unwrap();
    assert_eq!(last.len(), 5);
    assert!(last.iter().all(|e| e["status"] == "completed"));
    assert!(
        c.of_kind("tool_call")
            .iter()
            .any(|t| t["kind"] == "execute" && t["status"] == "completed")
    );
    let text = c.texts("agent_message_chunk");
    assert!(text.contains("**Planner:** needs hello.txt"), "{text}");
    assert!(text.contains("✅ Done: 1 file(s) changed"), "{text}");
}

#[tokio::test]
async fn spec_interview_spans_prompt_turns() {
    let dir = tempfile::tempdir().unwrap();
    let design = r###"{"summary":"simple","section_edits":[{"section":"Architecture","content":"Print it."}]}"###;
    let llm = Arc::new(FakeLlm::new([
        r#"{"type":"question","text":"Who is greeted?"}"#,
        r##"{"type":"draft","spec":"# Greet\n\n## Goal\nGreet Alice.\n"}"##,
        design,
    ]));
    let mut c = Client::start(llm.clone());
    let sid = c.new_session(dir.path()).await;

    let r = c.prompt(&sid, "/spec greet").await;
    assert_eq!(r["stopReason"], "end_turn", "the question ends the turn");
    assert!(c.texts("agent_message_chunk").contains("Who is greeted?"));
    assert!(!dir.path().join("docs/features/greet.md").exists());

    let r = c.prompt(&sid, "Alice").await;
    assert_eq!(r["stopReason"], "end_turn");
    let spec = fs::read_to_string(dir.path().join("docs/features/greet.md")).unwrap();
    assert!(spec.starts_with("---\nstatus: approved\n---\n# Greet"));
    assert!(spec.contains("## Architecture\nPrint it."));
    assert_eq!(llm.requests()[1].messages.last().unwrap().content, "Alice");
    assert!(c.texts("agent_message_chunk").contains("✅ Spec approved"));
}

#[tokio::test]
async fn cancel_stops_a_build_and_resume_finishes_it() {
    let dir = project();
    let mut c = Client::start(Arc::new(FakeLlm::new([PLAN, WORK_OK, DOC])));
    let sid = c.new_session(dir.path()).await;

    // Leave the first permission request open, then cancel the turn.
    let pending = c
        .call(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": "/build greet create hello.txt"}]}),
            |_| None,
        )
        .await
        .unwrap_err();
    assert_eq!(pending["method"], "session/request_permission");
    let prompt_id = c.next_id;
    c.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}}))
        .await;
    // As the protocol requires, the client answers open permissions "cancelled".
    c.send(json!({"jsonrpc": "2.0", "id": pending["id"], "result": {"outcome": {"outcome": "cancelled"}}}))
        .await;
    let stop = loop {
        let m = c.next().await;
        if m["id"] == prompt_id {
            break m["result"]["stopReason"].clone();
        }
    };
    assert_eq!(stop, "cancelled");
    assert!(!dir.path().join("hello.txt").exists());
    let runs = kamili::list_runs(dir.path());
    assert!(runs[0].resumable, "{}", runs[0].state);

    // `/resume` asks again and finishes; the Planner is not called again.
    let r = c.prompt(&sid, "/resume").await;
    assert_eq!(r["stopReason"], "end_turn");
    assert_eq!(
        fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
        "hi\n"
    );
}
