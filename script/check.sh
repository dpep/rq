#!/usr/bin/env bash
#
# The pre-push gate: formatting, lints, tests. Stops at the first failure.
#
#     script/check.sh
#
# Why a script rather than a chain of cargo commands: a shell pipeline's exit
# status is its LAST command's, so `cargo clippy … | tail -1` reports success
# even when clippy failed. `pipefail` below makes that impossible here; don't
# filter cargo through head/tail when you're gating on the result.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# This machine's cargo came via Homebrew's keg-only rustup, so it may not be on
# PATH (see CLAUDE.md). Find it rather than making every caller prefix it.
if ! command -v cargo >/dev/null 2>&1; then
  export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
fi
command -v cargo >/dev/null 2>&1 || {
  echo "check: cargo not found (tried /opt/homebrew/opt/rustup/bin)" >&2
  exit 1
}

step() { printf '\n=== %s\n' "$*"; }

# Drop this crate's artifacts first. Cargo's fingerprint can wedge "fresh",
# after which it reports success without recompiling changed sources and the
# gate validates code that isn't the code you wrote. Only rq is rebuilt — the
# six tree-sitter grammars stay cached — so this costs a few seconds and makes
# a green run mean what it says.
step "clean (this crate only)"
cargo clean -p reference-query

step "fmt"
cargo fmt --check

step "clippy"
cargo clippy --all-targets -- -D warnings

step "tests"
cargo test

printf '\nall green\n'
