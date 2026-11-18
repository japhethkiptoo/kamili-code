#!/usr/bin/env bash
# Every check CI runs, in the same order. Run before pushing:  ./scripts/ci.sh
# Offline: no network or API key needed (tests use FakeLlm).
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n==> %s\n' "$*"; }

step "module boundaries";              ./scripts/check-module-deps.sh
step "formatting";                     cargo fmt --all --check
step "clippy (all features)";          cargo clippy --workspace --all-targets -- -D warnings
step "clippy (no default features)";   cargo clippy -p kamili-cli --no-default-features -- -D warnings
step "engine tests, offline build";    cargo test -p kamili --no-default-features
step "all tests";                      cargo test --workspace
step "minimal CLI build";              cargo build -p kamili-cli --no-default-features

printf '\nall checks passed\n'
