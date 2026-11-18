# Development guide

How to build Kamili, run its tests, and change it without breaking the architecture.

## Prerequisites

- **Rust 1.88 or newer.** Check with `rustc --version`. Install via [rustup](https://rustup.rs).
- `clippy` and `rustfmt`: `rustup component add clippy rustfmt`
- No API key is needed for development. The whole test suite runs offline.
- Optional, to test browser sign-in: Anthropic's `ant` CLI (`brew install anthropics/tap/ant`, or `go install github.com/anthropics/anthropic-cli/v2/cmd/ant@latest`).

## Building

All commands run from the workspace root (the directory containing `Cargo.toml` and `ARCHITECT.md`).

| Goal | Command | Output |
|---|---|---|
| Debug build, everything | `cargo build --workspace` | `target/debug/kamili` |
| Release binary | `cargo build --release -p kamili-cli` | `target/release/kamili` (~6 MB) |
| Engine only | `cargo build -p kamili` | library |
| Run without installing | `cargo run -p kamili-cli -- whoami` | |
| Install to `~/.cargo/bin` | `cargo install --path channels/kamili-cli` | `kamili` on your PATH |

### Feature flags

| Crate | Feature | Default | Turn off to… |
|---|---|---|---|
| `kamili` | `anthropic` | on | build the engine without the Claude client (with `openai` also off: only `FakeLlm`, no HTTP client) |
| `kamili` | `openai` | on | drop `OpenAiCompatLlm` (NVIDIA NIM, OpenRouter, Ollama, …) |
| `kamili-cli` | `anthropic` | on | drop the Claude client and `login`/`logout`/`whoami` |
| `kamili-cli` | `openai` | on | drop `[provider] kind = "openai"` support |
| `kamili-cli` | `acp` | on | drop the `acp` subcommand and the ACP crate |
| `kamili-cli` | `desktop` | **off** | turn **on** for `kamili desktop` (needs Rust 1.99+) |
| `kamili-desktop` | `ui` | on | test the bridge and UI state without compiling GPUI |

```sh
cargo build -p kamili-cli --no-default-features             # ~2 MB, no model client
cargo build -p kamili-cli --no-default-features -F anthropic # Claude, but no ACP
cargo build -p kamili-cli --no-default-features -F openai,acp # OpenAI-compatible only, with ACP
```

## Testing

### Run everything CI runs

```sh
./scripts/ci.sh
```

This is the single source of truth. `.github/workflows/ci.yml` only calls this script. It runs, in order:

1. Module boundaries (`scripts/check-module-deps.sh`)
2. `cargo fmt --check`
3. Clippy with `-D warnings`, with all features and with none
4. Engine tests built without the `anthropic` feature (proves the engine works offline)
5. All workspace tests
6. A minimal CLI build

If `./scripts/ci.sh` passes locally, CI will pass.

### Run a subset

```sh
cargo test --workspace                 # all tests
cargo test -p kamili                   # engine: unit + end-to-end flows
cargo test -p kamili --test flows      # end-to-end flows only
cargo test -p kamili spec::            # one module's unit tests
cargo test -p kamili-cli               # CLI: credential storage, order, login menu
cargo test -p kamili build_retries     # one test by name
cargo test -- --nocapture              # show println! output
```

### What the tests cover

| Where | Kind | Covers |
|---|---|---|
| `kamili/src/**/*.rs`, `#[cfg(test)]` | unit | spec parsing and round-trip, edit permissions, diffs, config defaults, path safety, working copy and conflict detection, agent JSON parsing and tool loop, tool budgets, request headers and tool request bodies, state transitions (including the exhaustive approval check), ignore-aware walk, grep, read ranges, outlines, repo map, edit tools |
| `kamili/tests/flows.rs` | end-to-end | build happy path, retry with feedback, failure leaves the tree untouched and saves a report, draft spec blocked, permission violations, rejected approval, spec session and Architect, skills, Worker editing existing code with tools, resume after failure, cancel and resume, runs list/clean/discard |
| `channels/kamili-cli/src/auth.rs` | unit | credential order, empty environment variables, file permissions (0600/0700), key masking, login menu choices |
| `channels/kamili-acp/tests/acp.rs` | protocol | an editor-like client over in-memory pipes: `initialize`, errors, commands, a full build with permission requests and diffs, a spec interview across prompt turns, cancel then `/resume` |

### How the end-to-end tests work

The engine depends on two swappable traits: `Llm` (the model) and `Channel` (the user interface). The tests replace both:

- **`FakeLlm`** replays scripted replies in order and records every request, so you can assert which model was called and with what prompt. It returns `LlmError::Exhausted` when it runs out of replies. That makes a test fail loudly if the pipeline calls the model more often than expected.
- **`TestChannel`** (in `tests/flows.rs`) approves or rejects every approval prompt, answers questions from a list, and records events.
- The sandbox is the real `process` backend running a tiny shell command (`sh -c "test -f hello.txt"`). Tests therefore exercise real file copying and real exit codes.

To add a flow test, script the replies in the order the pipeline asks for them:

```rust
let llm = Arc::new(FakeLlm::new([PLAN, WORK_OK, DOC]));   // Planner, Worker, Documentor
let out = engine(dir.path(), llm.clone()).build(build_request(dir.path()), &ch).await?;
```

Call order for a build: Planner → Worker → (on test failure: Tester → Worker)… → Documentor. For a spec session: one Analyst call per turn, then one Architect call after the draft (unless `[session] architect = false`).

To script tool calls, use `FakeReply::tool(name, json!({...}))` in the reply list. Each tool turn uses one reply, and the agent's final JSON is another. `llm.chat_requests()` shows tools, tool results and cache flags; see `worker_edits_existing_code_with_tools_guided_by_the_plan`. For resume and cancel, see `failed_run_resumes_without_repeating_finished_agents` and `cancelled_run_resumes_with_last`: cancel a `CancelToken` before the build, or let `FakeLlm` run out of replies to simulate an outage.

To test skills, write a `SKILL.md` under `<project>/.agents/skills/<name>/` in the test, list it under `[skills.required]`, and assert on `llm.requests()[i].system`. See `required_skills_reach_their_agents_only` in `tests/flows.rs`.

### The desktop channel

`kamili-desktop` depends on GPUI (through `gpui-kit`), which needs Rust 1.99 or newer and several GB of disk for a debug build. It is a workspace member but not a default member, so plain `cargo build` and `cargo test` skip it and keep working on the workspace's minimum Rust.

```sh
cargo +stable run -p kamili-cli -F desktop -- --project ../some-project desktop
cargo +stable test -p kamili-desktop                         # bridge, state, and headless window tests
cargo test -p kamili-desktop --no-default-features           # bridge and state only, no GPUI
```

The window tests (`ui::tests`) open a real window headlessly with `#[gpui_kit::test]`, click by element ID, and check what reached the engine side. Plain clickable `div`s need `.test_support()` to be found by ID; it does nothing in normal builds.

Dependency note: gpui-kit's optional GPUI Fast backend pins exact versions of some shared crates (for example `taffy`, `unicode-properties`), and Cargo resolves it into `Cargo.lock` even though it isn't built. If an update fails to resolve, re-resolve the subtree together: `cargo +stable update -p gpui-kit --recursive`.

### Testing the editor channel (ACP)

`cargo test -p kamili-acp` runs a scripted ACP client against the server with `FakeLlm` (see `Client` in `tests/acp.rs`). To try the real binary by hand, pipe JSON-RPC lines into it; nothing but protocol messages may appear on stdout:

```sh
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}' \
  | target/debug/kamili acp
```

In Zed, add the `agent_servers` entry from the README (pointing `command` at `target/debug/kamili`), and use **dev: open acp logs** to watch the traffic.

### Testing against the real Claude API

The automated tests never call the API. To try the real thing:

```sh
cargo build -p kamili-cli
target/debug/kamili login                 # browser sign-in or API key; or export ANTHROPIC_API_KEY=...
target/debug/kamili whoami                # free: lists one model, uses no tokens

# Throwaway project
mkdir -p /tmp/kamili-demo/docs/features && cd /tmp/kamili-demo
printf '[build]\ntest_command = ["sh", "-c", "test -f hello.txt"]\n' > .kamili.toml
/path/to/target/debug/kamili spec greeting
/path/to/target/debug/kamili build greeting "create hello.txt containing a greeting"
```

> **Costs money.** A build makes at least 3 model calls (Planner and Analyst on Opus). Use a throwaway project, and remember the `process` sandbox runs `test_command` with your permissions.

To test credentials without touching your real ones, point Kamili at a temporary config directory:

```sh
KAMILI_CONFIG_DIR=$(mktemp -d) target/debug/kamili whoami
```

This isolates only Kamili's saved key. Browser sign-in lives in the `ant` CLI's own store (`~/.config/anthropic/`), so `kamili login --browser` and `ant auth logout` affect your real sign-in.

The interactive login menu needs a terminal. To script it, use `expect`:

```sh
KAMILI_CONFIG_DIR=$(mktemp -d) expect -c '
  spawn target/debug/kamili login
  expect "Choice"; send "2\r"
  expect "API key: "; send "sk-ant-fake-key-1234\r"
  expect eof'
```

## Working on the code

### The rules that CI enforces

- **Module boundaries.** Inside the engine, `spec`, `llm`, `sandbox`, `config`, `skills` and `index` are leaf modules: they import nothing else internal. `agents` may use only `llm`. `engine` may use all of them. `api` only re-exports. `scripts/check-module-deps.sh` fails the build on any violation. It also rejects grouped imports like `use crate::{a, b}`, which would hide a dependency from the check. Write one `use crate::<module>::…` line per module.
- **Only `api` is public.** Channels (`kamili-cli`, `kamili-acp`) can use only what `kamili/src/api/mod.rs` re-exports. If a channel needs something new, add it to `api`, and think of it as a public commitment.
- **No warnings.** Clippy runs with `-D warnings`.

### Common changes

| To… | Edit | Remember |
|---|---|---|
| Change an agent's behaviour | `kamili/src/agents/prompts/<agent>.md` | Keep the JSON reply shape in sync with the structs in `agents/mod.rs` |
| Add a progress event | `Event` in `engine/channel.rs` | Enums are `#[non_exhaustive]`, so adding a variant is safe. Channels print unknown events with `Debug` |
| Add an exit code | `engine/error.rs` | Never renumber existing codes. Update the tables in README and ARCHITECT |
| Add a config key | `config/mod.rs` | Give it a default. Document it in `.kamili.example.toml` |
| Add a channel | new crate under `channels/` | Implement `kamili::Channel`. No engine change needed |
| Change what an agent may edit | `spec/mod.rs` (`DOC_SECTIONS`, `Editor::may_edit`) | Update `docs/features/README.md` |
| Change the build flow | `engine/state.rs` (states, `step`), then the action in `engine/pipeline.rs` (`Runner::act`) | Keep `step` pure, and extend the exhaustive approval test. Bump `FORMAT_VERSION` in `engine/runs.rs` if saved runs can't be read any more |
| Tune the code graph | `index/graph.rs` (`name_weight`, `COMMON_NAMES`, `STOP_WORDS`, `focus`) | Check the result with `kamili graph --focus "…"` on a real repo before and after. If you change what gets parsed or stored (outline patterns, identifier rules, `CachedFile`), bump `CACHE_VERSION` so old caches are ignored |
| Add an agent tool | `agents/tools.rs` (`ToolRequest`, its definition), `engine/tools.rs` (`ProjectTools::run`) | Cap the output. Read tools take paths through `index::checked` |
| Add an agent | `agents/mod.rs` (`AgentRole`, a method, a prompt in `prompts/`), `engine/skills.rs` (`config_key`), `config/mod.rs` (`ModelConfig`, `RequiredSkills`), `AgentName` in `engine/channel.rs` | Give it its own spec sections in `spec/mod.rs` if it edits the spec |
| Map a new engine event in the editor | `channels/kamili-acp/src/channel.rs` (`AcpChannel::event`) | Only protocol messages on stdout. Check the shape against agentclientprotocol.com |
| Add an editor command | `channels/kamili-acp/src/session.rs` (`COMMANDS`, `parse_command`, `Session::prompt`) | It's published to the editor automatically |
| Change login methods | `channels/kamili-cli/src/auth.rs` (`Method`, `choose_method`) | Credentials stay in the CLI; the engine only gets a `CredentialSource`. Update README "Logging in" |

### Debugging a build run

Every build is recorded in `.kamili/runs/<run-id>/` in the target project:

- `state.json`: the current state, the spec as revised, the plan summary and context brief, each attempt's Worker summary, test result and Tester feedback, the working copy's location, tokens and tool calls per agent, and the last error. `kamili runs show <id>` prints it.
- `journal.jsonl`: one line per state change (`from`, `input`, `to`), so you can see exactly where a run went.

A resumable run's working copy stays in `$TMPDIR/kamili-work/`; open it to see what the Worker wrote. Spec-session transcripts are in `.kamili/sessions/<feature>.json`.

## Troubleshooting

| Symptom | Cause |
|---|---|
| `error: unrecognized subcommand 'login'` | The binary was built with `--no-default-features`. Rebuild with `cargo build -p kamili-cli` |
| `not logged in` (exit 3) | No credential found. Run `kamili login`, or check `kamili whoami` |
| `whoami` shows the wrong source | An `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` in your shell takes precedence, or a saved key outranks the browser sign-in (`kamili logout` removes it) |
| `browser sign-in uses Anthropic's ant CLI, which is not installed` | Install `ant` (see Prerequisites), or use `kamili login --api-key` |
| `Buildfile: build.xml does not exist!` during browser sign-in | Apache Ant (the Java tool) is first on your PATH. Check with `which -a ant` |
| browser doesn't open (SSH, headless) | Use `kamili login --browser --no-browser` and open the printed link elsewhere |
| `stdin is not a terminal` (exit 2) on `login` | Pass `--browser`, or pipe a key with `--api-key-stdin` |
| `set [build] test_command` (exit 2) | Builds need a test command in `.kamili.toml` |
| exit 10, conflict | You edited a file the run was about to write. Nothing was written; run again |
| exit 130 | You pressed Ctrl+C. `kamili build --resume` continues |
| `working copy … no longer exists` on resume | The temp dir was cleaned (e.g. reboot). `kamili runs discard <id>` and start again |
| The graph seems out of date | Delete `.kamili/cache/`; it's rebuilt on the next run. `kamili graph` shows how many files it parsed vs. took from cache |
| `kamili graph` misses a file | Only languages with outlines are in the graph (see README); check `.gitignore`, `.kamiliignore` and `[context]` |
| An agent can't find a file you know exists | It's ignored by `.gitignore`, `.kamiliignore` or `[context]`, or under `target`/`node_modules` |
| `let` chains / edition errors | Rust is older than 1.88. Run `rustup update` |
