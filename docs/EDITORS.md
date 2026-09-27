# Editor integration

rq is editor-independent: every result is a `path:line`, so any editor can
jump to it. That's the whole contract. No daemon, no socket — just a CLI call.
Everything below is a thin wrapper around it.

## Native (works today)

`rq -o/--open <query>` opens the best match for you (prompting to choose on a
TTY with several). The launcher resolves
`RQ_OPEN` (a template with `{file}`/`{line}`/`{}` = `path:line`) → `code` →
`$VISUAL`/`$EDITOR` → printing the location. Simplest integration: bind a key to
`rq -o`. The wrappers below remain useful for an interactive fzf picker or a
custom flow.

## Shell (works today)

[`script/rq-open`](../script/rq-open) does search → pick → open:

```sh
rq-open RefundProcessor
```

It uses `fzf` to pick when available (auto-selecting a lone match), opens in VS
Code (`code --goto`) or `$EDITOR`. Drop it on your `PATH`, or wire a shell
function / key binding to it. It's ~30 lines around `rq` — copy and adapt
freely.

## VS Code

### Extension

[`editors/vscode/`](../editors/vscode) is a small extension that makes
Cmd/Ctrl-click, F12 and Peek Definition go through rq. It is not on the
Marketplace; build and install the `.vsix` locally:

```sh
cd editors/vscode
npm install
npm run package                               # → rq-0.1.0.vsix
code --install-extension rq-0.1.0.vsix
```

It needs `rq` on VS Code's `PATH`, or `rq.path` set. If rq can't be run, it says so once, with a
button to the setting.

**How a click becomes a query.** The word under the cursor, plus the receiver
written right before it — no parsing, just the characters on the line:

| Cursor on               | Queries, in order                     |
| ----------------------- | ------------------------------------- |
| `widget.save`           | `save`                                |
| `Account::Ledger`       | `Account::Ledger`, then `Ledger`      |
| `Widget.find`           | `Widget.find`, then `find`            |
| `Widget.new`            | `Widget.new`, then `Widget`           |
| `self.save`, `@w.save`  | `save`                                |
| `w.empty?`, `w.save!`   | `empty?`, `save!` (Ruby)              |

A lowercase receiver is a variable whose class the extension can't know, so it
is dropped; a constant or a `::` path scopes the query, and the bare name is the
fallback for an inherited or mixed-in method. `--lang` is set from the file's
language (TypeScript and JavaScript together). Only exact, case-sensitive names
are kept — rq's fuzzy neighbours are right for a search box, wrong for a jump.
When one match remains or rq's top confidence is ≥ 0.8, the click jumps straight
there; otherwise up to five candidates show as a peek list.

rq runs in the file's repo root (the nearest `.git` ancestor, as rq itself
resolves it), falling back to the workspace folder outside git, with
`--wait 2s` and a 5 s kill: a cold repo answers with what's indexed and keeps
warming in the background, and a `warming` miss shows a status-bar note rather
than an error. A request VS Code cancels kills its rq process.

**Settings.**

| Setting        | Default         | Meaning                                                  |
| -------------- | --------------- | -------------------------------------------------------- |
| `rq.path`      | `"rq"`          | The rq binary.                                           |
| `rq.languages` | every rq language | Languages rq answers Go to Definition and Cmd-T for.   |
| `rq.mode`      | `"fallback"`    | `fallback`: answer only where no other provider does. `always`: answer alongside them. |
| `rq.workspaceSymbols` | `"dedupe"` | Cmd/Ctrl-T: `dedupe` adds only symbols no language server returned; `off` leaves it to them; `always` adds all. |

**Living with language servers.** VS Code merges every definition provider's
answers, and rq's ranges (the whole definition) never match a language server's
(the name), so where both answer, every definition appears twice and a click
opens a peek list instead of jumping. So by default rq asks the other providers
first and answers only when they find nothing: a precise server (rust-analyzer,
gopls, Pylance, TypeScript, Ruby LSP) keeps its answer, and rq fills the gaps —
Ruby metaprogramming (`delegate`, `has_many`, `scope`), a file the server hasn't
loaded, untyped code. The cost is waiting on the server first. Set `rq.mode` to
`always` to have rq answer every time.

**Also included.** Go to Symbol in Workspace (Cmd/Ctrl-T) from rq's ranked
index, for the same languages, and **rq: Search Definitions** in the command
palette — any rq query, every language, picked from a list.

**Developing it.** `npm test` runs the unit tests (the query builder, result
picking, the runner against a stub binary); `npm run test:e2e` launches a
downloaded VS Code on a Ruby fixture and asserts Go to Definition end to end
through the real `rq`.

### Task (no extension)

A `tasks.json` entry that prompts for a query and runs the wrapper:

```jsonc
{
  "version": "2.0.0",
  "tasks": [{
    "label": "rq: open",
    "type": "shell",
    "command": "rq-open ${input:rqQuery}",
    "problemMatcher": []
  }],
  "inputs": [{ "id": "rqQuery", "type": "promptString", "description": "rq query" }]
}
```

## Neovim

```lua
vim.keymap.set("n", "<leader>rq", function()
  local query = vim.fn.input("rq> ")
  if query == "" then return end
  local line = vim.fn.systemlist({ "rq", query })[1]   -- top hit
  if not line or line == "" then return end
  local loc = vim.split(line, "%s+")[1]                -- file:line
  local file, lnum = loc:match("([^:]+):(%d+)")
  vim.cmd(("edit +%s %s"):format(lnum, file))
end)
```
