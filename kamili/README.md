# Kamili

Kamili is a spec-driven coding agent. You agree on a feature spec with an AI analyst. Kamili then builds changes against that spec in a sandboxed copy of your project, tests them, and writes them back only when the tests pass.

- **The spec is the contract.** Each feature has a markdown spec in `docs/features/`. Only a spec marked `approved` can be built.
- **Conversation only happens while writing the spec.** Builds are deterministic and non-interactive, apart from approval prompts.
- **Agents propose, the engine applies.** AI agents return structured proposals. Kamili validates them, applies them to a temporary copy, and writes back in one final step.

> **Status: early (0.1).** Both flows work end to end, but read [Limitations](#limitations) before pointing Kamili at a real project.

---

## Quick start

```sh
# 1. Install (needs Rust 1.88+)
cargo install --path channels/kamili-cli

# 2. Log in to Claude
kamili login            # sign in with your browser, or paste an API key
kamili whoami           # confirm which credential is in use

# 3. Configure your project
cd ~/my-project
cp /path/to/kamili/.kamili.example.toml .kamili.toml
#    then set at least:  [build] test_command = ["cargo", "test"]

# 4. Write a spec with the Analyst, then build against it
kamili spec user-auth
kamili build user-auth "add password reset by email"
```

---

## How it works

### `kamili spec <feature>`: write the contract

The Analyst interviews you one question at a time and challenges vague requirements. When it has enough, it drafts the spec, with anything still unresolved under *Open Questions*. The **Architect** then adds an *Architecture* section: how the feature will be built (components, interfaces, decisions and risks), fitted to your repository's layout. You see the diff and approve it. Only then is `docs/features/<feature>.md` written with `status: approved`.

Answer with an empty line or `/quit` to pause. Continue later with `kamili spec <feature> --resume`. No code runs during a spec session.

### `kamili build <feature> "<task>"`: build against it

```text
load spec ─► Planner ─► Worker ─► tests ─► Documentor ─► write back
 (approved?)  (spec edits)  ▲        │        (change log)   (one step, after approval)
                            └─ Tester feedback ◄─ fail (up to max_retries)
```

1. **Load spec.** A draft spec stops here with exit code 6, unless you pass `--force`.
2. **Planner** explores the code (see below), proposes minimal spec edits for the task, and writes a *context brief* for the Worker: the files, symbols and tests that matter. You see the spec edits as a diff and approve them.
3. **Worker** starts from the brief, reads what it needs, and edits a **copy** of the project with targeted find-and-replace edits.
4. **Tests** run `build.test_command` on the copy. On failure, the **Tester** condenses the output into feedback and the Worker tries again.
5. **Documentor** updates the spec's *Change Log*, *API Contracts* and *Ops Notes* sections.
6. **Write back.** You approve. Kamili checks that nothing it will overwrite changed on disk during the run, then writes everything in one step.

If the run fails at any point, nothing is written to your project.

### How agents read a large codebase

Agents never get the whole repository. The Architect, Planner and Worker each start with a **repo map** that fits in `[context] repo_map_bytes`:

1. README and manifest excerpts.
2. **Key symbols**: the files and declarations most relevant to *this* task, ranked by the code graph (below).
3. The directory tree with file counts, listed as deep as fits.

From there they look things up with read-only tools:

| Tool | Does |
|---|---|
| `list_dir`, `find_files` | browse folders, find files by glob |
| `grep` | regex search, up to 100 matches with line numbers (parallel; ~0.7 s over 10,000 files) |
| `outline` | a file's functions and types with line numbers (Rust, Python, JS/TS, Go, Java/Kotlin/C#, Ruby, PHP, Swift, C/C++) |
| `read_file` | a range of lines, at most 400 per call |
| `find_symbol` | where a name is declared |
| `references` | where a declared name is used (every caller, before changing it) |
| `related_files` | what a file uses and what uses it, with the linking names |

The Worker also gets `replace_in_file`, `write_file` and `delete_file`, which only change the copy and can never touch specs.

- **What agents can see:** `.gitignore` and `.kamiliignore` rules (no git repository needed), plus `[context] include`/`exclude` globs. `.git`, `.kamili`, `target` and `node_modules` are always hidden.
- **Limits:** each agent turn is capped at `max_tool_calls` and `max_read_bytes`. When the budget runs out, the agent must answer with what it has.
- **Caching:** tool conversations use prompt caching, so each step re-reads earlier ones from cache at a fraction of the cost.

### The code graph

Kamili builds a graph of your code for each agent turn, without a compiler or language server. Per-file scan results are cached in `.kamili/cache/graph.json`, so only changed files are re-read:

- **Nodes** are source files and the symbols they declare (from the outlines above).
- **Edges** go from file A to file B when A uses a name B declares. Distinctive names (`parse_config`, `UserService`) count more. Names declared in many files, used almost everywhere, or very common (`run`, `get`, `is_empty`) count much less.
- **Ranking** is PageRank, focused on the task: files and names the task, spec or Planner's brief mention come first, then the code they depend on.

What it's used for:
- **Ranking the key symbols** in each agent's repo map.
- **The `find_symbol`, `references` and `related_files` tools.**
- **The impact list at write-back:** unchanged files that use the changed code (`src/main.rs (uses greeting_text)`), so you know what else to look at before approving.

Explore it yourself:

```sh
kamili graph                                   # most central files and their key symbols
kamili graph --focus "add rate limiting to login"   # ranked for a task
kamili graph map --focus "…"                   # the exact repo map an agent would get
kamili graph symbol parse_config               # where it's declared and used
kamili graph file src/auth/session.rs          # what it uses, what uses it
kamili graph impact src/auth/session.rs        # who is affected if it changes
kamili graph dot --limit 30 | dot -Tsvg > graph.svg
```

Name matching is approximate. Two unrelated functions called `parse` look alike, and a method named like a standard-library one (`is_terminal`) can link files that don't really depend on each other. That's fine for ranking and navigation, because agents confirm by reading.

Measured on 7,922 source files (200 crates): about 3 s for the first build, then 0.8 s with the cache, which takes 23 MB. A typical project's cache is far smaller; Kamili's own (34 files) is 51 KB. Delete `.kamili/cache/` at any time; it's rebuilt on the next run.

### Runs: resume, cancel, inspect

Every build is a **run** with an explicit state (`planning`, `working (attempt 2)`, `awaiting approval to write back`, …) saved to `.kamili/runs/<run-id>/` before each step:

- **Ctrl+C** stops at the next safe point; the run is saved as *cancelled* (exit 130). A second Ctrl+C quits at once. Writing back is never interrupted.
- **Failures worth retrying** (model, network, sandbox, config, or a malformed agent reply) leave the run *resumable*.
- **`kamili build --resume`** continues the latest resumable run (or `--resume <run-id>`) from the saved state. Agent work already done, and paid for, is reused, and the working copy is kept between attempts.
- **`kamili runs`** lists runs with state and token use. `kamili runs show <id>` prints the full record. `runs discard <id>` drops one. `runs clean [--all]` deletes leftovers of finished runs.

Each run folder holds `state.json` (state, spec revisions, plan, context brief, attempts, test feedback, tokens per agent) and `journal.jsonl` (one line per state change). The working copy lives outside your project, in the system temp directory, so your build tools never see two copies.

`--docs-only` runs only the Planner and Documentor and touches only the spec.

### Who may edit what

| Agent | Model (default) | May change |
|---|---|---|
| Analyst | Opus | the whole spec except *Architecture*, with your approval |
| Architect | Opus | only *Architecture*, with your approval |
| Planner | Opus | spec sections except *Architecture* and the three below |
| Documentor | Haiku | *Change Log*, *API Contracts*, *Ops Notes* |
| Worker | Sonnet | project files, but never spec files |
| Tester | Sonnet | nothing; it writes feedback only |

These rules are enforced in code, not just in the prompts. See [docs/features/README.md](docs/features/README.md) for the spec format.

---

## Commands

| Command | What it does |
|---|---|
| `kamili spec <feature> [--resume]` | Write or revise a spec with the Analyst |
| `kamili build <feature> "<task>" [--force] [--docs-only]` | Build a task against an approved spec |
| `kamili build --resume [<run-id>]` | Continue a cancelled or failed run (default: the latest resumable one) |
| `kamili runs [list \| show <id> \| discard <id> \| clean [--all]]` | List, inspect and clean up build runs |
| `kamili graph [top \| map \| symbol <name> \| file <path> \| impact <paths> \| dot] [--focus "<task>"]` | Explore the code graph agents use |
| `kamili login [--browser [--no-browser] \| --api-key \| --api-key-stdin] [--no-verify]` | Log in to Claude: browser sign-in or an API key. With no flags it asks |
| `kamili logout` | Remove the saved key |
| `kamili skills [list \| install]` | Show the skills each agent requires, or install missing ones from skills.sh |
| `kamili whoami` | Show which credential is in use and check it works |
| `kamili acp` | Run as an agent inside an editor such as Zed (see [In your editor](#in-your-editor-acp)) |
| `kamili desktop` | Open the desktop app (needs a `-F desktop` build; see [Desktop app](#desktop-app)) |

Global flags:

| Flag | What it does |
|---|---|
| `--project <dir>` | Project root (default: `.`) |
| `-y`, `--yes` | Approve every approval prompt |
| `--non-interactive` | For CI: never prompt; approval prompts are rejected unless `--yes` is also given |

### In your editor (ACP)

`kamili acp` speaks the [Agent Client Protocol](https://agentclientprotocol.com), so editors such as Zed can run Kamili in their agent panel. Log in once in a terminal (`kamili login`), then add Kamili to Zed's `settings.json`:

```json
{
  "agent_servers": {
    "Kamili": {
      "type": "custom",
      "command": "kamili",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

With an [OpenAI-compatible provider](#other-model-providers-nvidia-openrouter-ollama), put its key in `env` instead of logging in, for example `"env": {"NVIDIA_API_KEY": "nvapi-…"}`.

Use the full path to `kamili` if it isn't on the PATH Zed sees (for example `~/.cargo/bin/kamili`). Each thread works on the project you have open, with that project's `.kamili.toml`.

| Type in the agent panel | What happens |
|---|---|
| `/spec <feature>` | The Analyst interviews you in the chat. Each question ends the turn; your next message is the answer. You approve the draft, with the Architect's design, as a diff |
| `/build <feature> <task>` | A build runs. The plan pane tracks Plan → Work → Test → Document → Write back. Agents' tool calls and test runs show as tool calls. Each approval shows the real file diffs and asks Approve / Reject |
| `/resume [run]` | Continue a cancelled or failed build |
| `/runs`, `/graph [focus]`, `/skills` | Look at runs, the code graph, required skills |
| Stop button | A build stops at the next safe point and stays resumable; a spec interview pauses |

Nothing is written to your project until you approve the write-back, and Kamili writes it directly to disk (the editor picks the changes up). If something looks wrong, Zed's **dev: open acp logs** command shows every message.

### Logging in

`kamili login` asks how you want to log in:

```text
How do you want to log in to Claude?
  1) Sign in with your browser (Claude Console account)
  2) Paste an existing API key
  3) Create an API key in your browser, then paste it
```

- **Browser sign-in** (`kamili login --browser`) opens platform.claude.com, where you pick an organization and workspace. It runs through Anthropic's `ant` CLI, which keeps and refreshes the token; Kamili never stores it. Install `ant` first: `brew install anthropics/tap/ant`, or `go install github.com/anthropics/anthropic-cli/v2/cmd/ant@latest`. Over SSH, add `--no-browser` to get a link to open on another machine.
- **API key** (`--api-key`, or `--api-key-stdin` for scripts) is checked against the API, then saved.

Both use a **Claude Console** account, billed per use. A Claude.ai subscription (Pro, Max, Team), the login that Claude Code's `/login` offers, doesn't include API access, and third-party tools such as Kamili can't use it.

Kamili uses the first credential it finds, in this order:

1. `ANTHROPIC_API_KEY`
2. `ANTHROPIC_AUTH_TOKEN` (OAuth bearer token)
3. The key saved by `kamili login`, in `~/.config/kamili/credentials.toml`, readable only by you
4. The browser sign-in (the active profile of the `ant` CLI)

A leftover `ANTHROPIC_API_KEY` in your shell overrides everything else. If `kamili whoami` shows an unexpected source, check for one.

### Desktop app

`kamili desktop` opens a native window for the project (built with [gpui-kit](https://gpui-kit.com)). It isn't in the default build, because GPUI is large and needs Rust 1.99 or newer:

```sh
cargo +stable install --path channels/kamili-cli -F desktop
cd ~/my-project && kamili desktop
```

- **Sidebar:** the feature specs in `docs/features/` with their status. Click one to fill in the feature name.
- **Build:** start a build, follow Plan → Work → Test → Document → Write back, the agents' messages and tool calls, and each test attempt. **Stop** cancels at the next safe point (the run stays resumable).
- **Spec:** the Analyst's interview, answered in a text box. **Pause** saves the session for **Resume**.
- **Review:** every approval shows the files it would write, with a diff for each, and waits for **Approve** or **Reject**. Closing the window rejects.
- **Runs:** past runs, with **Resume** for those that can continue. **Settings:** the provider, models and build settings from `.kamili.toml`.

The window uses the same `[provider]` and credentials as the CLI. A missing credential is reported when a job starts.

### Other model providers (NVIDIA, OpenRouter, Ollama)

Kamili can use any API that speaks OpenAI's Chat Completions format instead of Claude: NVIDIA NIM (build.nvidia.com, which has a free developer tier), OpenRouter, Groq, or a local server such as Ollama, vLLM or LM Studio. Set it per project in `.kamili.toml`. No Anthropic credential is needed:

```toml
[provider]
kind = "openai"
base_url = "https://integrate.api.nvidia.com/v1"
api_key_env = "NVIDIA_API_KEY"   # the variable holding the key; leave empty for local servers

[models]                          # model IDs from your provider (required: the defaults are Claude IDs)
analyst = "<large model>"
architect = "<large model>"
planner = "<large model>"
worker = "<coding model>"
tester = "<coding model>"
documentor = "<small model>"
```

Then `export NVIDIA_API_KEY=nvapi-…` and run `kamili whoami` to check the endpoint and key. Keys are only read from the environment, never from `.kamili.toml`.

- Pick models that support **tool calling** (see each model's card); every agent except the Documentor uses tools.
- Reasoning effort, prompt caching and thinking aren't part of this API, so `effort` is ignored.
- Free tiers are rate-limited (NVIDIA's is around 40 requests a minute), and one agent turn can make up to `max_tool_calls` calls, so builds are slower. Open models also tend to need more Worker retries than Claude.

`[provider] base_url` also works with `kind = "anthropic"`, to send Claude requests through a gateway or proxy.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | workspace or I/O failure |
| 2 | configuration or usage error |
| 3 | model call failed (including not logged in) |
| 4 | an agent returned an invalid proposal |
| 5 | the sandbox could not run |
| 6 | the spec is not approved |
| 7 | tests still failing after all retries |
| 8 | rejected at an approval prompt |
| 9 | spec missing or malformed, or an edit outside the agent's sections |
| 10 | conflict: your files changed during the run |
| 130 | cancelled with Ctrl+C; resume with `kamili build --resume` |

---

## Configuration

Kamili reads an optional `.kamili.toml` in the project root. Every key has a default except `build.test_command`, which builds require. The annotated reference is [.kamili.example.toml](.kamili.example.toml).

```toml
[build]
test_command = ["cargo", "test"]
max_retries = 3

[models]
worker = "claude-sonnet-5-5"
```

### Skills

You can require each agent to follow [skills.sh](https://skills.sh) skills: reusable instructions in the open `SKILL.md` format, such as your team's conventions or a framework's best practices.

```toml
[skills.required]
all = ["acme/eng-skills@house-style"]                              # every agent
architect = ["vercel-labs/agent-skills@web-design-guidelines"]
worker = ["acme/eng-skills@house-style", "acme/eng-skills@testing"]
```

```sh
kamili skills install     # runs `npx skills add <owner/repo> --skill <name> -y` for each missing skill
kamili skills             # ok/missing per agent
```

- Write each entry as `owner/repo@name` so Kamili can install it, or just `name` for a skill you put in `.agents/skills/<name>/` yourself.
- **Required means required.** If a listed skill isn't installed, `spec` and `build` stop with exit code 2 before calling any model, and print the install command.
- Each agent gets its skills' `SKILL.md` text added to its instructions. A skill can't override the reply format or the who-may-edit rules, which Kamili enforces in code.
- Kamili never runs a skill's scripts. Agents can't yet read a skill's extra files (`references/` and so on), only `SKILL.md`.
- Skills are third-party instructions given to agents that write your code. Read a skill before you require it, and commit `skills-lock.json` (written by the skills CLI) to pin what you installed.

### Codebase context

```toml
[context]
include = []            # globs agents may see; empty = everything not ignored
exclude = ["**/*.min.js", "fixtures/**"]
repo_map_bytes = 12000  # size of the overview each agent starts with
max_tool_calls = 60     # per agent turn
max_read_bytes = 600000 # tool output per agent turn
```

For a monorepo, scope agents to the part a feature lives in with `include`, or add paths to a `.kamiliignore` file (same syntax as `.gitignore`).

Kamili also writes to `.kamili/` in your project (runs and session transcripts). Add it to your `.gitignore`.

---

## Limitations

- **No real sandbox yet.** Tests run as a normal local process, with your network access and permissions. Kamili prints a warning on every build. Only use it on projects whose test command you trust. Container isolation is planned.
- **The code graph matches names, not types.** It has no compiler or language server behind it, so links are approximate (see above). It's rebuilt for each agent turn from a per-file cache: under a second for 8,000 files.
- **The working copy is a full copy.** Starting a build on a very large repository takes as long as copying it (copy-on-write cloning is planned).
- **Skills are instructions only.** Kamili loads `SKILL.md` but can't run a skill's scripts or open its other files.
- **In the editor, sessions can't be reloaded** from the thread history (ACP `session/load`). Use `/spec <feature> --resume` or `/resume` instead. Image and audio prompts aren't supported.
- **Model calls cost money.** Each build makes at least 3 model calls, plus 2 more for every retry.

---

## Repository layout

```text
kamili/                 engine library: all logic, no I/O to the user
  src/api/              the only public surface
  src/engine/           run state machine + runner, spec session, tools, workspace, Channel contract, errors
  src/spec/             spec parsing, ownership rules, diffs
  src/agents/           agent roles, tool definitions + prompts/*.md
  src/index/            codebase scanning: walk, grep, outline, code graph, repo map
  src/skills/           SKILL.md discovery (skills.sh format)
  src/llm/              Claude client and FakeLlm
  src/sandbox/          test execution
  src/config/           .kamili.toml
channels/kamili-cli/    the `kamili` binary: terminal + CI channels, login
channels/kamili-acp/    editor channel: ACP over stdio
channels/kamili-desktop/ desktop channel: native window (gpui-kit)
docs/                   development and release guides, feature specs
scripts/                ci.sh, check-module-deps.sh
```

## Documentation

- [ARCHITECT.md](ARCHITECT.md): architecture, module rules, Channel contract
- [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md): building, testing, contributing
- [docs/RELEASING.md](docs/RELEASING.md): versioning, release builds, delivery
- [docs/features/README.md](docs/features/README.md): spec file format
