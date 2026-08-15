#!/usr/bin/env bash
#
# Cut a release: bump, gate, push, wait for CI, tag, publish, brew, release
# page, skill.
#
#     script/release.sh 0.41.0
#     script/release.sh patch --summary "confidence gate stopped over-firing"
#     script/release.sh minor --dry-run
#
# Why a script: the chain is a dozen ordered steps across three repos (rq, the
# tap, the claude plugins), and it has been hand-run every release. No single
# step is hard; the risk is that step ten gets forgotten at the end of a long
# session and something silently ships stale. The skill copy is the usual
# casualty, and no test covers it.
#
# Every step asks whether it has already happened and skips if so, so a run
# that dies halfway — a network blip mid-publish, a formula that fails audit —
# is re-run with the same arguments and picks up where it stopped.
#
# Ordering that matters: CI has to be green before the tag exists, and the tag
# has to be on GitHub before its tarball can be hashed for the formula. The
# irreversible steps (publish, pushes) all come after the local gate.
#
# CHANGELOG convention here is *not* `## Unreleased` — the version's section is
# written at release time (see CLAUDE.md), so this script requires the section
# to exist already and refuses to invent one.

set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# Keg-only rustup: same fallback the gate uses.
if ! command -v cargo >/dev/null 2>&1; then
  export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
fi

TAP_DIR="${RQ_TAP_DIR:-$HOME/code/lib/homebrew-tools}"
SKILL_SRC="claude/rq-skill.md"
SKILL_DST="${RQ_SKILL_DST:-$HOME/code/lib/claude/plugins/code/skills/rq/SKILL.md}"
SKILL_REPO="$HOME/code/lib/claude"
SKILL_PLUGIN_MANIFEST="$SKILL_REPO/plugins/code/.claude-plugin/plugin.json"
CRATE="reference-query"
REPO="dpep/rq"

SUMMARY=""
DRY_RUN=false
VERSION_ARG=""

while [ $# -gt 0 ]; do
  case "$1" in
    --summary) SUMMARY="${2:?--summary needs a value}"; shift 2 ;;
    --dry-run) DRY_RUN=true; shift ;;
    -h|--help) sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "unknown flag: $1" >&2; exit 2 ;;
    *) VERSION_ARG="$1"; shift ;;
  esac
done

step()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
skip()  { printf '    (already done: %s)\n' "$*"; }
die()   { printf '\033[31mrelease: %s\033[0m\n' "$*" >&2; exit 1; }
run()   { if $DRY_RUN; then printf '    would run: %s\n' "$*"; else "$@"; fi; }

CURRENT="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$CURRENT" ] || die "can't read the current version from Cargo.toml"

# --- version -----------------------------------------------------------------
# Below 1.0 by policy: minor or patch only, never major (CLAUDE.md).

case "${VERSION_ARG:-}" in
  minor|patch)
    VERSION="$(python3 - "$CURRENT" "$VERSION_ARG" <<'PY'
import sys
major, minor, patch = (int(p) for p in sys.argv[1].split("."))
if sys.argv[2] == "minor":
    minor, patch = minor + 1, 0
else:
    patch += 1
print(f"{major}.{minor}.{patch}")
PY
)" ;;
  major) die "rq stays below 1.0 — minor or patch only" ;;
  [0-9]*.[0-9]*.[0-9]*) VERSION="$VERSION_ARG" ;;
  "") die "usage: script/release.sh <version | minor | patch> [--summary TEXT] [--dry-run]" ;;
  *) die "not a version or a bump: $VERSION_ARG" ;;
esac

TAG="v$VERSION"
# `:+` tests for non-empty, and DRY_RUN holds the string "false" — so the label
# has to branch on the value, not its presence.
if $DRY_RUN; then
  echo "releasing $CURRENT -> $VERSION (dry run)"
else
  echo "releasing $CURRENT -> $VERSION"
fi

# --- preflight ---------------------------------------------------------------
# Everything that would be annoying to discover at step ten.

step "preflight"
[ "$(git rev-parse --abbrev-ref HEAD)" = "main" ] || die "not on main"
# A dirty tree would let the gate pass on code the tag doesn't contain.
if ! git diff --quiet || ! git diff --cached --quiet; then
  die "working tree is dirty — commit or stash first"
fi
git fetch --quiet origin
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] ||
  die "main and origin/main have diverged — push or pull first"
grep -q "^## $VERSION " CHANGELOG.md ||
  die "CHANGELOG.md has no '## $VERSION' section — write it before releasing"
[ -d "$TAP_DIR" ] || die "homebrew tap not found at $TAP_DIR (set RQ_TAP_DIR)"
[ -f "$SKILL_DST" ] || die "plugin skill copy not found at $SKILL_DST (set RQ_SKILL_DST)"
[ -f "$SKILL_PLUGIN_MANIFEST" ] || die "plugin manifest not found at $SKILL_PLUGIN_MANIFEST"
command -v gh >/dev/null || die "gh is not installed"
echo "    on main; changelog has a $VERSION section; tap, skill and gh found"

# --- bump --------------------------------------------------------------------

step "bump to $VERSION"
if [ "$CURRENT" = "$VERSION" ]; then
  skip "Cargo.toml is already $VERSION"
elif $DRY_RUN; then
  echo "    would set version = \"$VERSION\" in Cargo.toml and update Cargo.lock"
else
  python3 - "$CURRENT" "$VERSION" <<'PY'
import sys
old, new = sys.argv[1], sys.argv[2]
p = "Cargo.toml"
s = open(p).read().replace(f'version = "{old}"', f'version = "{new}"', 1)
open(p, "w").write(s)
PY
  cargo update --package "$CRATE" --precise "$VERSION" >/dev/null
fi

# --- prove it ----------------------------------------------------------------

step "gate"
if $DRY_RUN; then
  echo "    would run script/check.sh"
else
  script/check.sh
fi

# --- commit and push ---------------------------------------------------------
# main goes up before the tag, because the tag should name a commit CI has
# blessed.

step "commit"
SUBJECT="Release $VERSION"
if [ -n "$SUMMARY" ]; then
  SUBJECT="Release $VERSION ($SUMMARY)"
fi
if $DRY_RUN; then
  echo "    would commit: $SUBJECT"
elif git diff --quiet && git diff --cached --quiet; then
  skip "nothing to commit"
else
  git add Cargo.toml Cargo.lock CHANGELOG.md
  git commit -F - <<EOF
$SUBJECT

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
fi

step "push main"
if ! $DRY_RUN && [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main 2>/dev/null || echo none)" ]; then
  skip "origin/main is already at this commit"
else
  run git push origin main
fi

# --- CI ----------------------------------------------------------------------
# Tagging a red commit is the one mistake this whole ordering exists to
# prevent, and hand-waiting for CI has meant re-authoring a polling loop every
# release.

step "wait for CI"
if $DRY_RUN; then
  echo "    would wait for CI to pass on this commit"
else
  SHA="$(git rev-parse HEAD)"
  DEADLINE=$(( $(date +%s) + 900 ))
  while :; do
    STATUS="$(gh run list --repo "$REPO" --commit "$SHA" --limit 1 \
      --json status,conclusion --jq '.[0] | "\(.status) \(.conclusion)"' 2>/dev/null || true)"
    case "$STATUS" in
      "completed success") echo "    green"; break ;;
      "completed "*)       die "CI failed on $SHA: $STATUS" ;;
      ""|"null null")      echo "    no run yet…" ;;
      # Braces required: the ellipsis is non-ASCII, and bash otherwise reads it
      # as part of the variable name.
      *)                   echo "    ${STATUS}…" ;;
    esac
    [ "$(date +%s)" -lt "$DEADLINE" ] || die "CI still not green after 15m — check $REPO"
    sleep 15
  done
fi

# --- tag ---------------------------------------------------------------------

step "tag $TAG"
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
  skip "tag $TAG exists"
else
  run git tag -a "$TAG" -m "$SUBJECT"
fi
run git push origin "$TAG"

# --- publish -----------------------------------------------------------------

step "cargo publish"
# crates.io answers 403 to a request with no User-Agent, which would read as
# "not published" and send a resumed run back into `cargo publish` — an error,
# not a no-op, so the run would die at a step it had already completed.
if curl -fsS -A "rq-release-script (github.com/dpep/rq)" \
  "https://crates.io/api/v1/crates/$CRATE/$VERSION" >/dev/null 2>&1; then
  skip "$CRATE $VERSION is on crates.io"
else
  run cargo publish
fi

# --- homebrew ----------------------------------------------------------------

step "homebrew formula"
FORMULA="$TAP_DIR/Formula/rq.rb"
[ -f "$FORMULA" ] || die "formula not found at $FORMULA"
if grep -q "$TAG.tar.gz" "$FORMULA"; then
  skip "formula points at $TAG"
elif $DRY_RUN; then
  echo "    would fetch the $TAG tarball, compute its sha256, and update $FORMULA"
else
  TARBALL="$(mktemp -t rq-release)"
  # The tag has to exist on GitHub for this to resolve, which is why the push
  # comes first. A 404 here means the push didn't land, not that the sha moved.
  curl -fsSL "https://github.com/$REPO/archive/refs/tags/$TAG.tar.gz" -o "$TARBALL"
  SHA256="$(shasum -a 256 "$TARBALL" | cut -d' ' -f1)"
  rm -f "$TARBALL"
  echo "    sha256 $SHA256"
  python3 - "$FORMULA" "$TAG" "$SHA256" <<'PY'
import re, sys
formula, tag, sha = sys.argv[1], sys.argv[2], sys.argv[3]
s = open(formula).read()
s = re.sub(r"/tags/v[0-9.]+\.tar\.gz", f"/tags/{tag}.tar.gz", s)
s = re.sub(r'sha256 "[0-9a-f]{64}"', f'sha256 "{sha}"', s, count=1)
open(formula, "w").write(s)
PY
fi

step "brew build, test, audit"
if brew list --versions rq 2>/dev/null | grep -q "^rq $VERSION$"; then
  skip "rq $VERSION is installed"
else
  run brew uninstall rq
  run brew install --build-from-source dpep/tools/rq
fi
run brew test dpep/tools/rq
run brew audit --strict --online dpep/tools/rq

step "push tap"
if git -C "$TAP_DIR" diff --quiet -- Formula/rq.rb; then
  skip "tap has no formula change to push"
else
  run git -C "$TAP_DIR" add Formula/rq.rb
  if ! $DRY_RUN; then
    git -C "$TAP_DIR" commit -F - <<EOF
rq $VERSION

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
  fi
  run git -C "$TAP_DIR" push
fi

# --- release page ------------------------------------------------------------

step "github release"
if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
  skip "release $TAG exists"
elif $DRY_RUN; then
  echo "    would create release $TAG from the changelog section"
else
  NOTES="$(mktemp -t rq-notes)"
  python3 - "$VERSION" > "$NOTES" <<'PY'
import sys
version = sys.argv[1]
s = open("CHANGELOG.md").read()
start = s.index(f"## {version} ")
rest = s[start:]
end = rest.index("\n## ", 1)
print(rest[:end].split("\n", 1)[1].strip())
PY
  TITLE="$TAG"
  if [ -n "$SUMMARY" ]; then
    TITLE="$TAG — $SUMMARY"
  fi
  gh release create "$TAG" --repo "$REPO" --title "$TITLE" --notes-file "$NOTES"
  rm -f "$NOTES"
fi

# --- skill -------------------------------------------------------------------
# The step most likely to be skipped by hand, and the one nothing else catches:
# a stale skill misinforms an agent for a whole release cycle, silently.

step "sync skill"
if $DRY_RUN; then
  if cmp -s "$SKILL_SRC" "$SKILL_DST"; then
    skip "plugin skill copy is current"
  else
    echo "    the plugin copy is stale; a real run updates, bumps and commits it"
  fi
else
  # A copy, not a transform. The two files are meant to be byte-identical, so
  # the staleness check is a plain `cmp` — a sync that rewrites its input is a
  # sync that can disagree with the check that decides whether it ran.
  cp "$SKILL_SRC" "$SKILL_DST"
  if git -C "$SKILL_REPO" diff --quiet; then
    skip "plugin skill copy is current"
  else
    # Bump the plugin's own version with it. `claude plugin update` compares
    # versions, not content, so a skill change that doesn't move it never
    # reaches anyone — it answers "already at the latest version" and keeps
    # serving the old file. That has happened.
    python3 - "$SKILL_PLUGIN_MANIFEST" <<'PY'
import json, sys
p = sys.argv[1]
with open(p) as f:
    d = json.load(f)
major, minor, patch = (int(x) for x in d["version"].split("."))
d["version"] = f"{major}.{minor + 1}.0"
with open(p, "w") as f:
    json.dump(d, f, indent=2)
    f.write("\n")
print(f"    plugin version -> {d['version']}")
PY
    git -C "$SKILL_REPO" add -A
    git -C "$SKILL_REPO" commit -F - <<EOF
rq skill: $VERSION

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
    git -C "$SKILL_REPO" push
  fi
fi

# --- done --------------------------------------------------------------------

step "released $VERSION"
cat <<EOF
  crates.io   https://crates.io/crates/$CRATE/$VERSION
  release     https://github.com/$REPO/releases/tag/$TAG
  brew        $(brew list --versions rq 2>/dev/null || echo 'not installed')

  Worth a look before you walk away:
    rq --version        the shipped binary, not the dev build
EOF
