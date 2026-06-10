# Editor integration

rq is editor-independent. Integration is two small things:

1. **Open** a result — every result is a `path:line`, so any editor can jump to it.
2. **Report** the open — call `rq --record` so ranking learns which result you
   wanted for a query (see "Learning" in the [README](../README.md)).

```sh
rq --record --file <path> --line <n> <query>
```

That's the whole contract. No daemon, no socket — just two CLI calls. Everything
below is a thin wrapper around them.

## Native (works today)

`rq -o/--open <query>` does both steps for you: it opens the best match (prompting
to choose on a TTY with several) and records the pick. The launcher resolves
`RQ_OPEN` (a template with `{file}`/`{line}`/`{}` = `path:line`) → `code` →
`$VISUAL`/`$EDITOR` → printing the location. Simplest integration: bind a key to
`rq -o`. The wrappers below remain useful for an interactive fzf picker or a
custom flow.

## Shell (works today)

[`script/rq-open`](../script/rq-open) does search → pick → open → record:

```sh
rq-open RefundProcessor
```

It uses `fzf` to pick when available (auto-selecting a lone match), opens in VS
Code (`code --goto`) or `$EDITOR`, and records the choice. Drop it on your
`PATH`, or wire a shell function / key binding to it. It's ~30 lines of `rq` +
`rq --record` — copy and adapt freely.

## VS Code

Two approaches, smallest first.

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

### Extension (richer)

A small extension gives a native picker and accurate recording. Sketch of the
command handler:

```ts
import { execFile } from "node:child_process";
import * as vscode from "vscode";

export function activate(ctx: vscode.ExtensionContext) {
  ctx.subscriptions.push(
    vscode.commands.registerCommand("rq.search", async () => {
      const query = await vscode.window.showInputBox({ prompt: "rq" });
      if (!query) return;
      const cwd = vscode.workspace.workspaceFolders?.[0].uri.fsPath;

      const lines = await run("rq", [query], cwd);          // ranked results
      const pick = await vscode.window.showQuickPick(lines); // "file:line  kind name"
      if (!pick) return;

      const [path, line] = pick.split(/\s+/)[0].split(":");
      // record the choice so ranking learns
      await run("rq", ["--record", "--file", path, "--line", line, query], cwd);
      // open at the line
      const doc = await vscode.workspace.openTextDocument(`${cwd}/${path}`);
      const ed = await vscode.window.showTextDocument(doc);
      const pos = new vscode.Position(Math.max(0, +line - 1), 0);
      ed.selection = new vscode.Selection(pos, pos);
      ed.revealRange(new vscode.Range(pos, pos));
    })
  );
}

const run = (cmd: string, args: string[], cwd?: string) =>
  new Promise<string[]>((res, rej) =>
    execFile(cmd, args, { cwd }, (e, out) =>
      e && (e as any).code !== 1 ? rej(e) : res(out.trim().split("\n").filter(Boolean))
    )
  );
```

Note the `{ cwd }` option so rq runs against the workspace regardless of the
extension host's working directory (rq resolves the repository from its own
working directory). A fuller extension could also record passive opens
(`onDidOpenTextDocument`) attributed to the last query — but explicit
record-on-pick is the high-signal event and the place to start.

## Neovim

```lua
vim.keymap.set("n", "<leader>rq", function()
  local query = vim.fn.input("rq> ")
  if query == "" then return end
  local line = vim.fn.systemlist({ "rq", query })[1]   -- top hit
  if not line or line == "" then return end
  local loc = vim.split(line, "%s+")[1]                -- file:line
  local file, lnum = loc:match("([^:]+):(%d+)")
  vim.fn.system({ "rq", "--record", "--file", file, "--line", lnum, query })
  vim.cmd(("edit +%s %s"):format(lnum, file))
end)
```
