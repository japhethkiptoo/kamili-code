# Kamili v2: Architecture

Kamili is a Cargo workspace with one engine crate and one crate per channel. The engine is a single library crate whose internals are strict modules. Channels (CLI, ACP, others) are separate crates that depend on the engine's public API, so the compiler enforces that the engine never depends on a channel.

---

## 1. Principles

- **Engine is one crate, channels are separate crates.** The channel boundary is enforced by the compiler. The boundaries inside the engine are enforced by import rules and a narrow public surface.
- **One-way dependencies.** Channels depend on the engine. The engine depends on its leaf modules. Nothing depends upward.
- **Side effects at the edges.** Each module that touches the outside world owns exactly one kind of contact. Everything else is pure logic.
- **Agents propose, the engine disposes.** Agents return structured proposals. Only the engine applies them, and only after validation.
- **The spec is the contract.** The feature markdown file is the single source of truth, and only an `approved` spec may be built.
- **Conversation only in the spec phase.** The build pipeline is deterministic and non-interactive.

---

## 2. Workspace layout

```text
kamili/                              Cargo workspace root
├── Cargo.toml                       workspace, shared dependency versions
├── docs/features/                   Kamili's own specs (dogfooding)
│
├── kamili/                          ENGINE: library crate
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs                   module declarations + re-exports the public API
│       ├── api/                     the ONLY public surface (see section 7)
│       ├── engine/                  orchestration
│       │   ├── state                build run state machine (pure)
│       │   ├── pipeline             runner: performs each state's action
│       │   ├── runs                 run records, journal, resume, cancel
│       │   ├── tools                runs agents' tool calls (read; write to the copy)
│       │   ├── session              spec session (Analyst <-> user, then Architect)
│       │   ├── skills               required skills per agent -> prompt text
│       │   ├── workspace            working copy, write-back, run reports
│       │   ├── channel              Channel trait + Event/Approval/Decision types
│       │   └── error                unified error + exit codes
│       ├── spec/                    find, parse, validate, diff
│       ├── llm/                     model access: trait, Anthropic and OpenAI-compatible clients, fake
│       ├── agents/                  roles + prompts/*.md
│       ├── sandbox/                 isolated execution: trait, container backend
│       ├── skills/                  SKILL.md discovery and parsing (skills.sh format)
│       ├── index/                   codebase scanning: ignore-aware walk, grep, outline, code graph, repo map
│       └── config/                  .kamili.toml loading
│
└── channels/
    ├── kamili-cli/                  BINARY crate (the `kamili` executable)
    │   └── terminal channel + composition root
    ├── kamili-acp/                  LIBRARY crate
    │   └── ACP channel for editors such as Zed
    └── kamili-desktop/              LIBRARY crate
        └── desktop channel: native window (gpui-kit)
```

Naming notes: the orchestration module is called `engine`, not `core`, because `core` clashes with Rust's built-in `core` crate. The engine crate is called `kamili`; the executable is built by `kamili-cli` and named `kamili`.

---

## 3. Dependency rules

### Between crates (compiler-enforced)

```text
kamili-cli ──► kamili-acp ──► kamili (engine)
     └──────────────────────► kamili (engine)
```

| Crate | Depends on | Must not depend on |
|---|---|---|
| `kamili` (engine) | external libraries only | any channel |
| `kamili-acp` | `kamili`, `serde_json`, `tokio` (ACP is implemented directly; no SDK) | `kamili-cli`, other channels |
| `kamili-cli` | `kamili`, `kamili-acp` (optional feature), terminal libraries | |

Channels see only the engine's public API. They cannot reach into `spec`, `agents`, or `sandbox` internals, because those are not exported. From `llm` and `config`, only the construction types are exported (see the `api` row below).

### Between modules inside the engine (convention + CI)

| Module | May use | Must not use |
|---|---|---|
| `spec`, `llm`, `sandbox`, `config`, `skills`, `index` | nothing internal (leaf modules) | each other, agents, engine |
| `agents` | `llm` | spec, sandbox, engine |
| `engine` | spec, llm, agents, sandbox, config, skills, index | nothing above it |
| `api` | re-exports from `engine`, plus construction types from `config` (`Config`, `load_config`) and `llm` (`Llm` trait, `AnthropicLlm`, `OpenAiCompatLlm`, `FakeLlm`) | spec, agents, sandbox |

Channels are composition roots: they load config and pick the model client, so those types must be public. Nothing else from the leaf modules is.

```text
engine ──► spec | llm | sandbox | config
   └─────► agents ──► llm
```

**Enforcement inside the engine**

- Each module exposes a small deliberate surface and keeps everything else private or crate-private.
- A CI check (`scripts/check-module-deps.sh`) scans `crate::<module>` references and fails on any pair the table forbids. Grouped `use crate::{…}` imports are rejected so that no reference escapes the check.
- Each leaf module defines its own error type. Only `engine` combines them into the exit-code error.
- `Llm`, `Sandbox`, and `Channel` are traits, so real and fake implementations are swappable in tests.

---

## 4. How each module behaves

| Module | Role | Touches the outside world | Never does |
|---|---|---|---|
| **engine** | Sequences pipelines: decides what happens next, when to retry, when to write back. Maps failures to exit codes. | Workspace copies and run reports in `.kamili/` | Print, read input, speak a protocol, call the network or a container directly |
| **spec** | Loads specs, reads `status`, splits sections, validates proposed edits against ownership rules, produces diffs | Reads/writes spec files, only when the engine asks | Call a model, decide whether to build |
| **llm** | Sends a request to a model, returns text or tool calls. Keeps assistant content blocks verbatim. Model IDs come from config. A fake replays canned answers and tool calls. | Network | Know what an agent or a spec is |
| **agents** | Turns role + context into a prompt, runs the tool loop within a budget, and parses the final reply into a structured result (edits, questions, verdicts). Prompts are reviewable markdown files. | None directly: model calls go through llm, tool calls through the engine's `ToolHost` | Read files, run code, talk to the user |
| **index** | Walks a tree honouring `.gitignore`/`.kamiliignore`/scope, globs, greps (in parallel), reads line ranges, outlines symbols, builds the code graph (definitions, references, focused PageRank) and a size-capped repo map | Reads files | Write anything, decide what an agent may see |
| **sandbox** | Runs a command against a copy of the workspace: no network, CPU/memory/time limits. Returns exit code and output. | Container runtime | Judge whether results are good |
| **config** | Loads optional `.kamili.toml` and merges defaults | Reads the config file | Hold state |
| **api** | The stable surface channels use | None | Contain logic |

**Spec is the rule-keeper.** Status checks, who may edit which section, and diff generation all live in `spec`, so agents cannot bypass them.

---

## 5. Agents

| Agent | Model tier | Phase | Responsibility | Output (a proposal) |
|---|---|---|---|---|
| Analyst | Opus | Spec session | Interview, challenge, draft the spec with the user | Questions, draft spec or diff |
| Architect | Opus | Spec session | Design how the drafted spec will be built: components, interfaces, decisions, risks | Edits to `Architecture` only |
| Planner | Opus | Build | Reconcile the task with the spec; minimal edits (not `Architecture`) | Section edits |
| Worker | Sonnet | Build | Implement code matching the revised spec | File changes |
| Tester | Sonnet | Build | Interpret sandbox output | Verdict and retry feedback |
| Documentor | Haiku | Build | Finalise change log, API contracts, ops notes | Doc edits |

Agents never apply anything. Their output is validated and applied by the engine.

### Skills

Each agent can be required to follow skills: directories with a `SKILL.md` in the open format used by skills.sh. They are installed with the skills.sh CLI (`npx skills add owner/repo --skill name`, or `kamili skills install`) into `.agents/skills/`. `.kamili.toml` lists the skills per agent under `[skills.required]` (`all` applies to every agent).

- The engine resolves them at the start of every run. A missing skill fails the run with exit 2, before any model call, and prints the install command.
- A skill's `SKILL.md` body is appended to that agent's system prompt, after its role instructions. The role instructions and reply format take precedence, and the who-may-edit rules are enforced in code, so a skill cannot widen what an agent may change.
- Kamili never runs a skill's scripts. Agents can't yet open a skill's other files (`references/` and so on); that needs the tool-use work in the codebase-scanning plan.
- Each agent's skill text is capped by `[skills] max_bytes`. The run report lists the skills each agent loaded.

---

## 6. Flows

### Build run (`build <feature> "<task>"`)

A build is a state machine (`engine/state.rs`), driven by a runner (`engine/pipeline.rs`):

```text
Planning ─► [AwaitingPlanApproval] ─► Working(n) ─► Testing(n) ─pass─► Documenting
                                         ▲              │fail
                                         └─ Diagnosing(n)◄┘ (n ≤ max_retries)
Documenting ─► [AwaitingWriteApproval] ─► WritingBack ─► Done
any ─► Failed | Cancelled   (both may keep a state to resume from)
```

- **Pure decisions.** `state::step(state, input, policy)` decides the next state and does no I/O. Unexpected inputs become an internal failure, not a panic. Tests check every path, including an exhaustive search proving `WritingBack` is entered only with approval.
- **Actions in the runner.** For each state the runner performs one action (call an agent, run the tests, ask for approval, write back) and turns the outcome into an input.
- **Saved before every action.** The whole run (`Run`: state, spec revisions, plan, context brief, attempts, feedback, the working copy's location and fingerprints, usage per agent) goes to `.kamili/runs/<id>/state.json`, and each transition is appended to `journal.jsonl`.
- **Resume.** `Failed` (for retryable errors: exit 1–5) and `Cancelled` keep a `resume_from` state. `build --resume` reloads the run, re-resolves skills, and continues there. Agent replies already recorded are never requested again.
- **Cancel.** A `CancelToken` interrupts any state's action except `WritingBack`.
- **Working copy.** It lives in the system temp dir (`kamili-work/<run>-<project hash>`), outside the project, so build tools and ignore files of the real tree never see it. It records a fingerprint per touched file, so conflict checks and write-back work after a resume.

Steps:

1. A channel hands the engine a build request (or a run to resume).
2. Engine asks `spec` to load the feature and check it is `approved`. A draft stops here (exit 6) unless forced. Required skills are resolved before any model call.
3. **Planning:** the Planner gets the spec, task and repo map, explores with read-only tools, and returns spec edits plus a context brief. `spec` validates the edits.
4. **AwaitingPlanApproval** (if the spec changed and `approve_spec_edits`): diff shown, channel decides.
5. **Working:** the Worker gets the spec, brief and the copy's repo map, reads and edits the copy through tools. The engine's `ToolHost` refuses spec files, protected directories and escaping paths.
6. **Testing:** `sandbox` runs the tests on the copy.
7. **Diagnosing:** on failure, the Tester condenses the output into feedback and the machine returns to Working, up to the retry limit.
8. **Documenting**, then **AwaitingWriteApproval**, then **WritingBack**: one final step after conflict checks. On failure nothing is written; the run record stays.

`--docs-only` skips Working, Testing and Diagnosing.

### How agents see the codebase

No agent receives the whole tree. The Architect, Planner and Worker get a repo map capped at `[context] repo_map_bytes` (root manifests and README excerpt, then the directory tree listed as deep as fits). They use tools (`list_dir`, `find_files`, `grep`, `outline`, `read_file`; the Worker also `replace_in_file`, `write_file`, `delete_file`) within per-turn limits (`max_tool_calls`, `max_read_bytes`). Once a limit is hit, the agent must answer, and a further tool call fails the turn.

- **Results are capped:** 100 grep matches, 400 lines per read, 500 listed entries.
- **Tool conversations use prompt caching**, so each round reads the previous rounds from cache.
- **The Planner's context brief** is the handoff: the more expensive model finds the code once, and the Worker starts from it.

### The code graph (`index/graph.rs`)

Built per agent turn from the outlines, with no parser or language server:

- **Nodes and edges.** Files and their declared symbols are the nodes. File A links to file B when A mentions a name B declares (`mod` declarations excluded).
- **Edge weights.** Each mention counts √count × name weight. The name weight favours distinctive names. It is reduced for names declared in many files, mentioned by a large share of files, or on a short list of very common method names. Names declared in more than 40 files are ignored.
- **Focus.** Taken from the task, spec or brief: declared names it mentions, paths, and file or module stems (a `mod.rs`, `index.ts` or `__init__.py` takes its directory's name; stop words and ambiguous stems are skipped).
- **Ranking.** Personalised PageRank, with 97% of the restart probability on focus files. Focus files then rank first, followed by what they depend on. A file's rank is shared among its symbols by incoming weight.
- **Uses.** The `<key_symbols>` part of the repo map, the `find_symbol`, `references` and `related_files` tools, the write-back impact list (`Run.impact`), and `kamili graph`.
- **Freshness.** The Worker's graph is rebuilt on the next graph tool call after an edit, re-parsing only the edited files.
- **Cache.** `GraphCache` holds per-file results: named declarations, and mentions of project-declared names, as numbers into a shared identifier table. It is stored as compact JSON in `.kamili/cache/graph.json`. Entries are reused when size and modification time match, or else when the contents' fingerprint matches; that is how a working copy (new times, same contents) reuses the project's cache. The cache is saved only after building the real project, never a working copy. The `index` module only serialises it; the engine does the file I/O. Bump `CACHE_VERSION` when the outline or identifier rules change.

Known limits: matching is by name, not type. A name newly declared in one file links unchanged files that already used it only once they change. Tree-sitter parsing is the upgrade path for precision.

### Spec session (`spec <feature>`)

1. Engine loads context (existing spec, related specs, repo layout, all read-only).
2. Loop between the Analyst and the channel: interview, challenge, summarise periodically, up to a round cap.
3. Analyst drafts the spec or a diff, with unresolved items under Open Questions.
4. The Architect writes the draft's `Architecture` section from the spec and the repository layout (skipped if `[session] architect = false`).
5. Engine asks the channel for approval. Only on approval does `spec` write the file and set `status: approved`.
6. Transcript is saved to `.kamili/sessions/`. Optionally continue into a build.

The sandbox is never involved, and no code is executed.

### Failure behaviour

- Typed errors travel up to the engine, which maps each to a stable exit code.
- No partial changes: all work happens on the copy, and write-back is one final step.
- A failed or cancelled build keeps its record (and, if resumable, its working copy) for `kamili runs` and `--resume`.
- Channels present errors; they do not decide what they mean.

---

## 7. The public API and the Channel contract

The engine crate exports one deliberate surface (`api`), and nothing else is public. It contains:

- **The Channel contract:** the trait channels implement, plus its event, approval, and decision types.
- **Entry points:** start a build, start or resume a spec session, with their request and outcome types.
- **Errors:** the unified error and its exit codes.
- **Construction:** how to build an engine from configuration and a model client.

### The Channel contract

| Direction | Purpose |
|---|---|
| Engine to channel: events | One-way progress: stage started/finished, agent messages, diffs, test results, logs. Must not block the pipeline. |
| Engine to channel: question | Free-text question to the user (Analyst interview). "No answer" ends the session or signals a non-interactive channel. |
| Engine to channel: approval | A gate before something irreversible (writing a spec, applying changes). Returns approve or reject. Default is reject. |
| Channel to engine: requests | Start a build, start or resume a spec session. |

`Channel` is an async trait (via `async-trait`, so it stays usable as `dyn Channel`). `event` is synchronous and must return immediately. `ask` and `approve` are async, so a channel such as ACP can suspend until the editor's next prompt. The engine runs on tokio. The contract's enums are `#[non_exhaustive]`, so channels must handle unknown variants, and adding a variant is not a breaking change.

### Exit codes

Stable. Never renumber; only add.

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | workspace or I/O failure |
| 2 | configuration or usage error, or feature not supported |
| 3 | model call failed (including missing credentials) |
| 4 | agent returned an invalid proposal (malformed, unsafe path, Worker touching a spec) |
| 5 | sandbox could not run |
| 6 | spec is not approved |
| 7 | tests still failing after all retries |
| 8 | rejected at an approval gate |
| 9 | spec missing or malformed, or an edit outside the editor's sections |
| 10 | conflict: the real tree changed during the run |
| 130 | cancelled (Ctrl+C); the run can be resumed |

### Channels

- **`kamili-cli`:** events become terminal output, questions read stdin, approvals are a y/N prompt. It is also the composition root: it reads config, builds the engine, picks the model client, and runs the requested channel. It owns Claude credentials: `kamili login` saves an API key (checked against the API, stored owner-only in `~/.config/kamili/credentials.toml`), or `kamili login --browser` signs in to a Claude Console account in the browser through Anthropic's `ant` CLI (Kamili never stores that token; `ant` keeps and refreshes it). With no flags, `kamili login` asks which method to use. Credentials are resolved in this order: `ANTHROPIC_API_KEY`, then `ANTHROPIC_AUTH_TOKEN`, then the saved key, then the `ant` profile. `kamili whoami` shows which one is in use, and `kamili logout` removes the saved key. The engine's `AnthropicLlm` only receives a `CredentialSource`. It never reads environment variables or files. The CLI's `provider` module picks the client from the project's `[provider]` table: `anthropic` (default; the credentials above, optionally a `base_url` gateway) or `openai`, an `OpenAiCompatLlm` for any Chat Completions API (NVIDIA NIM, OpenRouter, Ollama, …) with its key read from the environment variable named in `api_key_env`. ACP builds engines through the same function, so it needs no Anthropic credential when the project uses `openai`. `OpenAiCompatLlm` translates the engine's Anthropic-style content blocks to Chat Completions messages and back, and drops effort, caching and thinking blocks.
- **`kamili-desktop`:** a native window built with gpui-kit (GPUI). `kamili desktop` runs it on the main thread. Engine jobs run on the CLI's tokio runtime, and each job builds its engine through the same `EngineFactory` pattern as ACP, so a missing credential is reported in the window instead of stopping it from opening. `bridge` implements `Channel`: `event` pushes to an unbounded channel the window reads, and `ask` and `approve` send a oneshot and wait for the user. A dropped reply (window closed, job finished) means no answer or `Reject`. `state` turns messages into what is shown: build steps, an activity feed, test attempts, the open question, and the open approval with per-file diffs. Both modules are GPUI-free and unit-tested; `ui` has headless window tests (`#[gpui_kit::test]`). The crate is excluded from the workspace's `default-members` because GPUI is large.
- **`kamili-acp`:** ACP protocol version 1, implemented directly as newline-delimited JSON-RPC 2.0 on stdio (no SDK: Kamili uses a small part of the protocol, and this avoids SDK churn). `kamili acp` starts it before any project config is loaded. Each `session/new` gets its own engine for its `cwd`, built by the CLI's `EngineFactory` (config plus credentials; missing credentials give ACP's auth-required error, -32000). How it maps:
  - **Commands:** `session/prompt` carries `/spec`, `/build`, `/resume`, `/runs`, `/graph`, `/skills` and `/help`, published as available commands.
  - **Jobs across turns:** a spec session or build runs as a job that can span several prompt turns. An Analyst question ends the turn (`end_turn`), and the next plain prompt answers it. A new command abandons an open question.
  - **Events:** become `session/update`s: messages, thoughts, a five-step `plan` driven by `Event::StateChanged.step`, and finished tool calls for agent tools and test runs.
  - **Approvals:** each becomes a `tool_call` with `diff` content built from `ApprovalRequest.changes` (absolute paths, full old and new text), then `session/request_permission` with Approve and Reject options. Anything but an explicit Approve rejects.
  - **Cancel:** `session/cancel` cancels a build's `CancelToken` (the run stays resumable) or pauses a spec session, and the turn returns `cancelled`.
  - **Not supported:** `session/load`.
  - **Output:** nothing but protocol messages goes to stdout.
- **Non-interactive (CI):** a policy-driven variant where questions return nothing and approvals follow a configured policy. It can live in `kamili-cli`.

The engine never decides which channel runs.

---

## 8. Features and builds

| Crate | Feature | Default | Effect |
|---|---|---|---|
| `kamili` | `anthropic` | yes | Real model client. Without it only the fake is compiled, which suits offline tests. |
| `kamili` | `openai` | yes | OpenAI-compatible client (`OpenAiCompatLlm`). |
| `kamili-cli` | `acp` | yes | Includes `kamili-acp` and the `acp` subcommand. Disable for a smaller binary. |
| `kamili-cli` | `desktop` | no | Includes `kamili-desktop` and the `desktop` subcommand. Needs Rust 1.99+ (GPUI). |
| `kamili-desktop` | `ui` | yes | The window. Without it only the bridge and UI state compile (fast tests). |

Channel-specific dependencies live in the channel crates, so the engine builds light and tests offline.

---

## 9. Contract stability and releases

Because channels are separate crates, the engine's public API is a real interface.

- **Keep the API small.** Everything not in `api` stays crate-private.
- **Add events and fields additively.** Removing or renaming anything in the contract is a breaking change.
- **Version together at first.** Use one workspace version for all crates while the API settles, then version the engine independently once channels are published separately.
- **Extending the set of channels** means adding a new crate that implements the contract. Slack, a web UI, or a CI runner need no engine change.
- **Third parties.** If outsiders may write channels, treat the contract as a public product from the start. If not, keep it internal and let it change freely.
- **Non-Rust channels** would need a process boundary (JSON-RPC over stdio or a socket). This is optional, and not needed while all channels are Rust.

---

## 10. Trade-offs

**Gains:** the compiler guarantees channels cannot reach engine internals and the engine cannot depend on a channel. Heavy channel dependencies stay out of the engine. Channels can have their own release cadence.

**Costs:** a workspace to maintain, an API surface to keep stable, and cross-cutting changes (a new event type) touch two crates. Boundaries inside the engine still depend on convention and CI, not the compiler.

**Watch:** `engine/workspace` is the module most likely to grow. If it does, split it into its own module tree first, then a crate only if it earns it.

---

## 11. Open decisions

- [ ] Whether third parties may write channels (decides how strictly the API is versioned)
- [ ] Whether `engine/workspace` becomes its own module tree or crate once it grows
- [ ] Policy for approving Analyst edits to an existing spec: always, or only when removing or changing requirements
- [ ] Retry limit defaults, and whether repeated failures can escalate to the Planner
- [ ] Sandbox technology (container vs microVM) and default resource limits
- [x] ACP SDK choice and how plan and diff updates are mapped: no SDK; see the `kamili-acp` bullet in §7
- [ ] Whether `kamili-cli` hosts the non-interactive CI channel or it gets its own crate
- [ ] Whether to offer a separate-process protocol for non-Rust channels

### Interim choices in the initial skeleton

These are placeholders, not final answers to the decisions above. Each can be revisited.

- **Analyst edits:** the Analyst always produces a full draft, and the user always approves it before it is written.
- **Planner edits to an approved spec:** shown as a diff and gated by `build.approve_spec_edits` (default on). The spec stays `approved`.
- **Retries:** default `max_retries = 3`. No escalation to the Planner yet.
- **Code context:** search plus a name-based code graph (regex outlines, identifier references, focused PageRank). No language server, tree-sitter or semantic index yet. Graph scan results are cached per file in `.kamili/cache/`. The working copy is a full copy (no copy-on-write yet).
- **Run state:** builds only. Spec sessions still resume from their saved transcript, not from the state machine.
- **Sandbox:** `process` backend only. It has **no isolation**, and the engine warns on every build. `container` is a stub that errors.
- **CI channel:** lives in `kamili-cli` (`--non-interactive`, with `--yes` to approve).
- **Write-back:** a run first checks that every file it will write, and the spec, are unchanged since the working copy was made. If anything changed, it stops with exit 10 and writes nothing.
