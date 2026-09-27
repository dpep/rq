// Pure logic, no `vscode` import: turning the text at the cursor into rq
// queries, and rq's JSON into the definitions worth showing. Unit-tested
// under node; extension.ts is the thin VS Code wiring around it.

import * as path from "node:path";

/** One result row from `rq --json`. Only the fields the extension reads. */
export interface Hit {
  name: string;
  kind: string;
  file: string; // repo-root-relative
  root?: string; // the checkout `file` is relative to (rq ≥ 0.53); absent from older rq
  line: number; // 1-based
  end_line?: number;
  parent?: string;
  confidence: number;
  signature?: string;
}

/** A query to run, and the definition names that count as an answer to it. */
export interface Attempt {
  query: string;
  names: string[];
}

/** What to ask rq for the symbol under the cursor, most specific first. */
export interface Lookup {
  attempts: Attempt[];
  /** Columns of the full expression (`Account::Ledger`), for the Cmd-hover underline. */
  start: number;
  end: number;
}

// Receivers that name "the current object/module": the enclosing scope is
// unknown without parsing, so the member is looked up bare.
const SELF = new Set(["self", "this", "Self", "crate", "super"]);

const CONSTRUCTORS = ["initialize", "__init__", "constructor", "new"];

/** A result at or above this confidence is taken alone, so Cmd-click jumps. */
export const CONFIDENT = 0.8;
/** Most definitions offered when rq isn't sure — VS Code shows them as a peek list. */
export const MAX_DEFINITIONS = 5;

export function buildLookup(line: string, character: number, languageId: string): Lookup | undefined {
  const ruby = languageId === "ruby";
  const js = /^(type|java)script/.test(languageId);
  const isIdent = (ch: string | undefined) =>
    ch !== undefined && (/[\p{L}\p{N}_]/u.test(ch) || (js && ch === "$"));
  const isSuffix = (ch: string | undefined) => ruby && (ch === "?" || ch === "!");

  // The word at the cursor, or just before it (the cursor may sit at the word's end).
  let pos = character;
  if (!isIdent(line[pos])) {
    if (isIdent(line[pos - 1])) pos -= 1;
    else if (isSuffix(line[pos]) && isIdent(line[pos - 1])) pos -= 1;
    else if (isSuffix(line[pos - 1]) && isIdent(line[pos - 2])) pos -= 2;
    else return;
  }
  let start = pos;
  while (isIdent(line[start - 1])) start--;
  let end = pos;
  while (isIdent(line[end])) end++;
  // Ruby predicates and bang methods (`empty?`, `save!`) — but not `!=`.
  if (isSuffix(line[end]) && line[end + 1] !== "=") end++;

  const word = line.slice(start, end);
  if (/^\p{N}/u.test(word)) return;
  // Ruby @ivar, @@cvar, $global: variables, not definitions rq indexes.
  if (ruby && (line[start - 1] === "@" || line[start - 1] === "$")) return;

  // Walk back over a receiver chain, keeping the part that names a scope rq
  // can resolve: `A::B` segments always, `.` segments only when capitalized
  // (`Widget.new` yes, `widget.save` no — a variable's class is unknown).
  let scope = "";
  let receiver: string | undefined;
  let i = start;
  for (;;) {
    let sep: string;
    let j: number;
    if (line.slice(i - 2, i) === "::") [sep, j] = ["::", i - 2];
    else if (line[i - 1] === ".") {
      [sep, j] = [".", i - 1];
      if (js && line[j - 1] === "?") j--; // optional chaining `Foo?.bar`
    } else break;

    let k = j;
    while (isIdent(line[k - 1])) k--;
    const ident = line.slice(k, j);
    if (!ident || /^\p{N}/u.test(ident) || SELF.has(ident)) break;
    if (ruby && (line[k - 1] === "@" || line[k - 1] === "$")) break;
    if (sep === "." && !/^\p{Lu}/u.test(ident)) break;

    scope = ident + sep + scope;
    receiver ??= ident;
    i = k;
  }

  let attempts: Attempt[];
  if (!receiver) {
    attempts = [{ query: word, names: [word] }];
  } else if (word === "new") {
    // `Widget.new` → rq resolves the constructor, or the class when it's
    // inherited. The bare class covers a scope rq can't place.
    attempts = [
      { query: scope + word, names: [...CONSTRUCTORS, receiver] },
      { query: receiver, names: [receiver] },
    ];
  } else {
    // Scoped first; the bare name when the scope misses (an inherited method, a mixin).
    attempts = [
      { query: scope + word, names: [word] },
      { query: word, names: [word] },
    ];
  }
  return { attempts, start: i, end };
}

/**
 * The definitions to show for an attempt. rq's list also holds fuzzy matches
 * (`save_all` for `save`), which are right for a search box but wrong for Go
 * to Definition, so only exact, case-sensitive names survive.
 */
export function pickHits(hits: Hit[], names: string[]): Hit[] {
  const exact = hits.filter((h) => names.includes(h.name));
  if (exact.length === 0) return [];
  if (exact.length === 1 || exact[0].confidence >= CONFIDENT) return [exact[0]];
  return exact.slice(0, MAX_DEFINITIONS);
}

export type RqResult =
  | { status: "ok"; hits: Hit[] }
  | { status: "miss" }
  | { status: "warming"; hits: Hit[] }
  | { status: "error"; message: string };

/**
 * Interpret one rq run from its exit code (0 hit, 1 miss, 2 warming, anything
 * else an error — 64 for usage) and `--json` stdout.
 */
export function parseResult(code: number, stdout: string, stderr = ""): RqResult {
  if (code === 1) return { status: "miss" };
  if (code !== 0 && code !== 2) {
    return { status: "error", message: jsonError(stdout) ?? (stderr.trim() || `rq exited with status ${code}`) };
  }

  let json: unknown;
  try {
    json = JSON.parse(stdout);
  } catch {
    // an rq older than 0.54 exits 2 for a usage error too (e.g. a flag it doesn't know)
    if (!stdout.trim() && stderr.trim()) return { status: "error", message: stderr.trim() };
    return { status: "error", message: `rq printed unparseable JSON: ${stdout.slice(0, 200)}` };
  }
  const error = errorOf(json);
  if (error !== undefined) return { status: "error", message: error };
  // A miss or a warming index is a `{"status": …}` object, not a result array.
  const hits = Array.isArray(json) ? json.filter(isHit) : [];
  return code === 2 ? { status: "warming", hits } : { status: "ok", hits };
}

/** The message of rq's `{"error": …}` object on stdout, if that's what it printed. */
function jsonError(stdout: string): string | undefined {
  try {
    return errorOf(JSON.parse(stdout));
  } catch {
    return undefined;
  }
}

function errorOf(json: unknown): string | undefined {
  const e = (json as { error?: unknown } | null)?.error;
  return typeof e === "string" ? e : undefined;
}

function isHit(x: unknown): x is Hit {
  const h = x as Hit;
  return typeof h === "object" && h !== null && typeof h.name === "string" &&
    typeof h.file === "string" && typeof h.line === "number";
}

/**
 * The repo root rq will use for `dir`: the nearest ancestor holding `.git`
 * (mirrors `index::repo_root`), or undefined outside git. rq's result paths are
 * relative to this root.
 */
export function findRepoRoot(dir: string, exists: (p: string) => boolean): string | undefined {
  for (let d = path.resolve(dir); ; d = path.dirname(d)) {
    if (exists(path.join(d, ".git"))) return d;
    if (path.dirname(d) === d) return;
  }
}

/** Absolute path of a hit, given the directory rq ran in (its root). */
/** Absolute path of a hit: rq's own `root` when it reports one, else the directory rq ran in. */
export function hitPath(root: string, hit: Hit): string {
  return path.resolve(hit.root ?? root, hit.file);
}

/** Keys another provider's symbol claims: its file with its line, and with its name. */
export function symbolKeys(file: string, line0: number | undefined, name: string): string[] {
  return line0 === undefined ? [`${file}#${name}`] : [`${file}:${line0}`, `${file}#${name}`];
}

/** Hits no other provider already returned — the same definition by file and line, or by name in that file. */
export function unseen(root: string, hits: Hit[], taken: Set<string>): Hit[] {
  return hits.filter((h) => !symbolKeys(hitPath(root, h), h.line - 1, h.name).some((k) => taken.has(k)));
}

const RQ_LANG: Record<string, string> = {
  ruby: "ruby",
  rust: "rust",
  go: "go",
  python: "python",
  typescript: "typescript",
  typescriptreact: "typescript",
  javascript: "javascript",
  javascriptreact: "javascript",
};

/** VS Code language ids an `rq.languages` entry covers: TS/JS include their JSX variants. */
export function editorLanguages(setting: string[]): string[] {
  return setting.flatMap((l) => (l === "typescript" || l === "javascript" ? [l, `${l}react`] : [l]));
}

/**
 * `--lang` value for a set of VS Code language ids. A definition referenced
 * from TypeScript may live in JavaScript and vice versa, so the two travel together.
 */
export function rqLangs(languageIds: string[]): string {
  const out = new Set<string>();
  for (const id of languageIds) {
    const lang = RQ_LANG[id];
    if (!lang) continue;
    if (lang === "typescript" || lang === "javascript") out.add("typescript").add("javascript");
    else out.add(lang);
  }
  return [...out].join(",");
}
