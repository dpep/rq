import * as fs from "node:fs";
import * as path from "node:path";
import * as vscode from "vscode";
import { buildLookup, editorLanguages, findRepoRoot, Hit, hitPath, pickHits, rqLangs, symbolKeys, unseen } from "./query";
import { runRq, RunResult } from "./rq";

let log: vscode.OutputChannel;
let missingReported = false;
// rq before 0.53 rejects --anchor; learned on first refusal, reset when rq.path changes
let anchorUnsupported = false;
// Positions whose definition we're asking the other providers for; the ask
// re-enters our own provider, which must stay out of its answer.
const asking = new Set<string>();

export function activate(ctx: vscode.ExtensionContext) {
  log = vscode.window.createOutputChannel("rq");

  // The selector is fixed at registration, so a change to rq.languages re-registers.
  let definitions: vscode.Disposable | undefined;
  const registerDefinitions = () => {
    definitions?.dispose();
    const selector = editorLanguages(languages()).map((language) => ({ language, scheme: "file" }));
    definitions = vscode.languages.registerDefinitionProvider(selector, { provideDefinition });
  };
  registerDefinitions();

  ctx.subscriptions.push(
    log,
    { dispose: () => definitions?.dispose() },
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("rq.languages")) registerDefinitions();
      if (e.affectsConfiguration("rq.path")) missingReported = anchorUnsupported = false;
    }),
    vscode.languages.registerWorkspaceSymbolProvider({ provideWorkspaceSymbols }),
    vscode.commands.registerCommand("rq.search", search),
  );
}

export function deactivate() {}

async function provideDefinition(
  doc: vscode.TextDocument,
  pos: vscode.Position,
  token: vscode.CancellationToken,
): Promise<vscode.LocationLink[] | undefined> {
  const lookup = buildLookup(doc.lineAt(pos.line).text, pos.character, doc.languageId);
  if (!lookup) return;
  if (mode() === "fallback" && (await othersAnswer(doc, pos))) return;
  const cwd = rootFor(doc.uri);
  const origin = new vscode.Range(pos.line, lookup.start, pos.line, lookup.end);

  for (const { query, names } of lookup.attempts) {
    const res = await rq(query, ["-l", "10", "--lang", rqLangs([doc.languageId])], cwd, token, anchorFor(doc, pos));
    if (res.status === "miss") continue;
    if (!("hits" in res)) return; // cancelled, or a failure already reported

    const picks = pickHits(res.hits, names);
    if (picks.length) return picks.map((h) => link(cwd, h, origin));
    if (res.status === "warming") {
      // A fallback would only ask the same incomplete index.
      vscode.window.setStatusBarMessage("rq: still indexing this repo — try again in a moment", 4000);
      return;
    }
  }
}

/** Whether another definition provider (a language server) has an answer here. */
async function othersAnswer(doc: vscode.TextDocument, pos: vscode.Position): Promise<boolean> {
  const key = `${doc.uri}:${pos.line}:${pos.character}`;
  if (asking.has(key)) return true; // the re-entrant call: defer to the outer one
  asking.add(key);
  try {
    const defs = await vscode.commands.executeCommand<unknown[]>("vscode.executeDefinitionProvider", doc.uri, pos);
    return (defs?.length ?? 0) > 0;
  } finally {
    asking.delete(key);
  }
}

/** Where the other workspace-symbol providers (language servers) already point, for this query. */
async function othersSymbols(query: string, key: string): Promise<Set<string>> {
  asking.add(key);
  try {
    const found =
      (await vscode.commands.executeCommand<vscode.SymbolInformation[]>("vscode.executeWorkspaceSymbolProvider", query)) ??
      [];
    // a lazily resolved symbol may carry only its file, so it's matched by name
    return new Set(
      found.flatMap((s) => symbolKeys(s.location.uri.fsPath, s.location.range?.start.line, s.name)),
    );
  } finally {
    asking.delete(key);
  }
}

async function provideWorkspaceSymbols(
  query: string,
  token: vscode.CancellationToken,
): Promise<vscode.SymbolInformation[]> {
  const cwd = workspaceRoot();
  const langs = rqLangs(languages());
  const how = symbolsMode();
  if (!query.trim() || !cwd || !langs || how === "off") return [];
  const key = `symbols:${query}`;
  if (asking.has(key)) return []; // the re-entrant call below: the outer one answers

  const [res, taken] = await Promise.all([
    rq(query, ["-l", "50", "--lang", langs], cwd, token),
    how === "dedupe" ? othersSymbols(query, key) : Promise.resolve(new Set<string>()),
  ]);
  if (!("hits" in res)) return [];
  return unseen(cwd, res.hits, taken).map(
    (h) =>
      new vscode.SymbolInformation(
        h.name,
        symbolKind(h.kind),
        h.parent ?? "",
        new vscode.Location(vscode.Uri.file(hitPath(cwd, h)), new vscode.Position(h.line - 1, 0)),
      ),
  );
}

/** `rq.search`: type a query, pick from rq's ranked list, open it. Every language. */
async function search() {
  const cwd = workspaceRoot();
  if (!cwd) {
    vscode.window.showWarningMessage("rq: open a folder or file to search.");
    return;
  }
  const query = await vscode.window.showInputBox({ prompt: "rq", placeHolder: "Widget, Account::Ledger, Widget#save" });
  if (!query) return;

  const res = await rq(query, ["-l", "30"], cwd);
  if (res.status === "miss" || ("hits" in res && res.hits.length === 0)) {
    const why = res.status === "warming" ? " yet — rq is still indexing this repo" : "";
    vscode.window.showInformationMessage(`rq: no definition matches "${query}"${why}.`);
    return;
  }
  if (!("hits" in res)) return; // missing, cancelled, or an error already logged

  const items = res.hits.map((hit) => ({
    label: hit.name,
    description: [hit.kind, hit.parent].filter(Boolean).join(" · "),
    detail: `${hit.file}:${hit.line}`,
    hit,
  }));
  const pick = items.length === 1 ? items[0] : await vscode.window.showQuickPick(items, { matchOnDetail: true });
  if (!pick) return;

  const pos = new vscode.Position(pick.hit.line - 1, 0);
  await vscode.window.showTextDocument(vscode.Uri.file(hitPath(cwd, pick.hit)), {
    selection: new vscode.Range(pos, pos),
  });
}

/** Run rq, reporting failures once and quietly; callers see only usable results. */
/** `--anchor FILE:LINE` for a position: rq ranks the enclosing class and nearby files first. */
function anchorFor(doc: vscode.TextDocument, pos: vscode.Position): string | undefined {
  return doc.uri.scheme === "file" ? `${doc.uri.fsPath}:${pos.line + 1}` : undefined;
}

async function rq(
  query: string,
  extra: string[],
  cwd: string,
  token?: vscode.CancellationToken,
  anchor?: string,
): Promise<RunResult> {
  const abort = new AbortController();
  const sub = token?.onCancellationRequested(() => abort.abort());
  const bin = vscode.workspace.getConfiguration("rq").get<string>("path") || "rq";
  try {
    let res: RunResult;
    if (anchor && !anchorUnsupported) {
      res = await runRq(bin, query, [...extra, "--anchor", anchor], cwd, abort.signal);
      if (res.status === "error" && res.message.includes("--anchor")) {
        anchorUnsupported = true;
        log.appendLine("rq doesn't know --anchor (older than 0.53); ranking without the click position");
        res = await runRq(bin, query, extra, cwd, abort.signal);
      }
    } else {
      res = await runRq(bin, query, extra, cwd, abort.signal);
    }
    if (res.status === "missing") reportMissing(bin);
    if (res.status === "error") log.appendLine(`rq ${JSON.stringify(query)} in ${cwd}: ${res.message}`);
    return res;
  } finally {
    sub?.dispose();
  }
}

function reportMissing(bin: string) {
  if (missingReported) return;
  missingReported = true;
  const msg =
    `rq: couldn't run "${bin}". Install rq (brew install dpep/tools/rq) or point the rq.path setting at it.`;
  vscode.window.showErrorMessage(msg, "Open Settings").then((choice) => {
    if (choice) vscode.commands.executeCommand("workbench.action.openSettings", "rq.path");
  });
}

function link(root: string, hit: Hit, origin: vscode.Range): vscode.LocationLink {
  const line = hit.line - 1;
  const endLine = Math.max(line, (hit.end_line ?? hit.line) - 1);
  return {
    originSelectionRange: origin,
    targetUri: vscode.Uri.file(hitPath(root, hit)),
    targetRange: new vscode.Range(line, 0, endLine, Number.MAX_SAFE_INTEGER),
    targetSelectionRange: new vscode.Range(line, 0, line, 0),
  };
}

/**
 * Directory to run rq in. rq roots itself at the enclosing git repo and
 * reports paths relative to it; outside git it roots at its cwd, so the
 * workspace folder is the better fallback than the file's own directory.
 */
function rootFor(uri: vscode.Uri): string {
  const dir = path.dirname(uri.fsPath);
  return findRepoRoot(dir, fs.existsSync) ?? vscode.workspace.getWorkspaceFolder(uri)?.uri.fsPath ?? dir;
}

function workspaceRoot(): string | undefined {
  const active = vscode.window.activeTextEditor?.document.uri;
  if (active?.scheme === "file") return rootFor(active);
  const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  return folder && (findRepoRoot(folder, fs.existsSync) ?? folder);
}

function languages(): string[] {
  return vscode.workspace.getConfiguration("rq").get<string[]>("languages") ?? [];
}

function symbolsMode(): "dedupe" | "off" | "always" {
  return vscode.workspace.getConfiguration("rq").get<"dedupe" | "off" | "always">("workspaceSymbols") ?? "dedupe";
}

function mode(): "fallback" | "always" {
  return vscode.workspace.getConfiguration("rq").get<"fallback" | "always">("mode") ?? "fallback";
}

function symbolKind(kind: string): vscode.SymbolKind {
  switch (kind) {
    case "class":
      return vscode.SymbolKind.Class;
    case "module":
      return vscode.SymbolKind.Module;
    case "method":
      return vscode.SymbolKind.Method;
    case "struct":
      return vscode.SymbolKind.Struct;
    case "enum":
      return vscode.SymbolKind.Enum;
    case "trait":
      return vscode.SymbolKind.Interface;
    default:
      return vscode.SymbolKind.Function;
  }
}
