# Releasing and delivering Kamili

How to cut a version and get the `kamili` binary to users.

> **Current state:** releases are manual. There is no release automation, no published crates, and no signed binaries yet. The section [Automating later](#automating-later) lists what to add when needed.

## What gets delivered

One executable, `kamili`, built from `channels/kamili-cli`. The engine (`kamili`) and the ACP channel (`kamili-acp`) are compiled into it. Users need nothing else at runtime except:

- a Claude credential (`kamili login`)
- for browser sign-in (`kamili login --browser`) only: Anthropic's `ant` CLI on their PATH

All crates are `publish = false`. Nothing goes to crates.io unless you decide to change that. If you do, see ARCHITECT.md §9 on API stability first.

## Versioning

- All crates share one version, set once in the root `Cargo.toml` under `[workspace.package] version`. ARCHITECT.md §9 says to version them together until the engine API settles.
- Follow semver. While the version is `0.x`, a minor bump (`0.1` → `0.2`) may break things.
- **Breaking changes** include:
  - removing or renaming anything exported from `kamili/src/api`
  - renumbering an exit code
  - changing the spec file format
  - changing the `.kamili.toml` keys or the credentials file location
- Adding events, exit codes or config keys is **not** breaking.

## Release checklist

1. **Start from a clean tree** on the main branch, up to date.
2. **Run every check:**
   ```sh
   ./scripts/ci.sh
   ```
3. **Smoke-test against the real API** (costs a little; see DEVELOPMENT.md):
   ```sh
   cargo build --release -p kamili-cli
   target/release/kamili whoami
   # run one `spec` and one `build` in a throwaway project
   ```
4. **Bump the version** in the root `Cargo.toml`, then refresh the lockfile:
   ```sh
   cargo update --workspace
   ```
5. **Update docs** if anything user-visible changed: README tables (commands, exit codes), `.kamili.example.toml`, ARCHITECT.md.
6. **Commit and tag:**
   ```sh
   git commit -am "Release vX.Y.Z"
   git tag -a vX.Y.Z -m "vX.Y.Z"
   git push && git push --tags
   ```
7. **Build release binaries** for each platform you ship (below).
8. **Publish:** attach the archives and checksums to the release (for example, a GitHub Release for the tag) with short release notes listing user-visible changes and any breaking changes.

## Building release binaries

```sh
cargo build --release --locked -p kamili-cli
# → target/release/kamili
```

`--locked` makes the build fail if `Cargo.lock` is out of date, so the binary uses exactly the dependency versions you tested.

### One platform per machine (simplest)

Build on each target OS (or in CI on a runner for each OS), then package:

```sh
VERSION=0.1.0
TARGET=$(rustc -vV | sed -n 's/^host: //p')       # e.g. aarch64-apple-darwin
NAME=kamili-$VERSION-$TARGET

mkdir -p dist/$NAME
cp target/release/kamili README.md dist/$NAME/
tar -C dist -czf dist/$NAME.tar.gz $NAME
shasum -a 256 dist/$NAME.tar.gz > dist/$NAME.tar.gz.sha256
```

On Windows the binary is `kamili.exe`; ship a `.zip` instead of a `.tar.gz`.

### Typical targets

| Platform | Target triple |
|---|---|
| macOS, Apple Silicon | `aarch64-apple-darwin` |
| macOS, Intel | `x86_64-apple-darwin` |
| Linux x86-64 | `x86_64-unknown-linux-gnu` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` |
| Windows x86-64 | `x86_64-pc-windows-msvc` |

You can cross-compile between the two macOS targets on one Mac:

```sh
rustup target add x86_64-apple-darwin
cargo build --release --locked -p kamili-cli --target x86_64-apple-darwin
# → target/x86_64-apple-darwin/release/kamili
```

Building Linux or Windows binaries from a Mac needs extra tooling (a cross linker, or a tool such as `cross`). Building on each OS in CI is usually simpler.

The HTTP client uses rustls (pure Rust TLS). The binary therefore doesn't depend on the system's OpenSSL.

### Smaller binaries

| Build | Size (macOS arm64, 0.1.0) |
|---|---|
| default (`anthropic` + `acp`) | ~6 MB |
| `--no-default-features -F anthropic` | ~6 MB (ACP is a stub today, so dropping it saves nothing yet) |
| `--no-default-features` | ~2 MB, no model client (testing only) |

To shrink further, you can add a release profile to the root `Cargo.toml`:

```toml
[profile.release]
lto = true
codegen-units = 1
strip = true
```

This isn't enabled yet. Measure the result and check the build time before adopting it.

## Ways users can install

| Method | Command (user side) | Needs |
|---|---|---|
| From source | `cargo install --path channels/kamili-cli` (in a clone) | Rust 1.88+ |
| From git | `cargo install --git <repo-url> kamili-cli` | Rust 1.88+, repo access |
| Prebuilt binary | download the archive, check the `.sha256`, put `kamili` on the PATH | nothing |

On macOS, unsigned binaries downloaded through a browser are quarantined by Gatekeeper. Users can run `xattr -d com.apple.quarantine ./kamili`. The proper fix is signing and notarising (see below).

## What must never ship

- **Credentials.** `kamili login` stores keys in the user's config directory (`~/.config/kamili/`), never in the repo or the build output. Don't add test keys to fixtures or CI logs.
- **`.kamili/` directories** from test runs (run reports and transcripts can contain code and prompts). They're in `.gitignore`.
- Debug builds. Always ship `--release --locked`.

## Automating later

When manual releases get tedious, these are the usual next steps:

1. **A release workflow** triggered by `v*` tags. It runs `./scripts/ci.sh`, builds on a matrix of OS runners, packages as above, and uploads to the release. Tools such as `cargo-dist` can generate this for you.
2. **macOS signing and notarisation** (needs an Apple Developer account), and Windows code signing.
3. **Package managers:** a Homebrew tap, or `cargo-binstall` metadata.
4. **A changelog** file maintained per release.

Each of these is a decision to make deliberately (cost, accounts, who maintains it), so none is set up yet.
