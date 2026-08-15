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

gate() {
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
}

# Keep the full run on disk. A failure here is often the first sighting of an
# intermittent one, and the name of the test that failed is the whole evidence
# — which a caller's `| tail` throws away exactly when it matters. `target/` is
# gitignored, and `cargo clean -p` doesn't touch this file.
mkdir -p target
LOG="target/check.log"

# Tee via `exec`, not `gate | tee`. A function on the LEFT of a pipe runs with
# `set -e` suppressed, so a failing step doesn't stop the run and the status
# you get back is the last step's, not the first failure's. That's the same
# trap the header warns about for `| tail`, and it is not hypothetical: this
# script reported "all green" over a failing clippy until it was rewritten
# this way.
exec > >(tee "$LOG") 2>&1

finish() {
  local rc=$?
  [ "$rc" -eq 0 ] && return 0
  printf '\ncheck FAILED — full output kept at %s\n' "$LOG"
  printf 'the failing test, unfiltered:\n'
  grep -E '^(test .* FAILED|failures:|---- .* stdout ----)' -A 3 "$LOG" || true
  return "$rc"
}
trap finish EXIT

gate

printf '\nall green\n'
