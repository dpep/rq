# rq for VS Code

Go to Definition (Cmd/Ctrl-click, F12, Peek) backed by
[rq](https://github.com/dpep/rq), the ranked code-navigation CLI. Strongest for
Ruby, where language servers struggle with metaprogramming — rq also knows the
methods `attr_accessor`, `delegate`, `has_many` and friends define.

## Requirements

`rq` 0.52 or newer on your `PATH` (`brew install dpep/tools/rq`), or set
`rq.path`. rq indexes the repo on first use; there is nothing else to set up.

## What it does

- **Go to Definition / Peek.** The word under the cursor, plus the receiver
  written right before it, becomes an rq query: `Widget.new` → the constructor,
  `Account::Ledger` → that nested class, `Widget.find` → `find` in `Widget` and
  then anywhere, `widget.save` → any `save` (a variable's class is unknown).
  Only exact names count. A confident answer jumps straight there; close
  candidates show as a peek list.
- **Go to Symbol in Workspace (Cmd/Ctrl-T)** from rq's ranked index.
- **rq: Search Definitions** in the command palette — any query rq accepts,
  across every language.

## Settings

| Setting        | Default           | Meaning                                                |
| -------------- | ----------------- | ------------------------------------------------------ |
| `rq.path`      | `"rq"`            | The rq binary.                                         |
| `rq.languages` | every rq language | Languages rq answers Go to Definition and Cmd-T for.   |
| `rq.mode`      | `"fallback"`      | `fallback`: answer only where no other provider does. `always`: answer alongside them. |

VS Code shows every definition provider's answers together, so where a language
server also answers, each definition would appear twice. In the default
`fallback` mode rq answers only where the language server finds nothing. See [docs/EDITORS.md](https://github.com/dpep/rq/blob/main/docs/EDITORS.md).
