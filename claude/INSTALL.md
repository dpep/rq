# Installing the rq skill

Two separate things: the **binary**, which does the work, and the **skill**,
which teaches Claude to drive it. You need both, and they install independently.

## The binary

```sh
brew install dpep/tools/rq      # macOS/Homebrew — builds from source, no runtime deps
```

No Homebrew:

```sh
cargo install reference-query   # the crate is `reference-query`; the binary is `rq`
```

Update with `brew upgrade dpep/tools/rq`, or re-run the `cargo install` line.

No setup after that: the first search in a repository indexes it. `rq --index`
warms it ahead of time, which is worth doing on a large monorepo.

## The skill

Two routes. They suit different people, so ask rather than picking.

### The marketplace plugin (the better default)

```
/plugin marketplace add dpep/claude
/plugin install code@dpep
```

One install, and `claude plugin update code@dpep` keeps it current.

Prefer this unless there's a reason not to. A skill file describing an older
binary than the one installed is the failure mode worth avoiding, and this is
the route that gets updates.

### A local copy

```sh
mkdir -p ~/.claude/skills/rq
cp claude/rq-skill.md ~/.claude/skills/rq/SKILL.md
```

Just this skill, nothing else — right when the user wants nothing else from the marketplace.

Either way, restart Claude Code after installation.
