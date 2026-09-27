import * as assert from "node:assert/strict";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { describe, it } from "node:test";
import { buildLookup, editorLanguages, findRepoRoot, Hit, hitPath, parseResult, pickHits, rqLangs, symbolKeys, unseen } from "../../src/query";

// The cursor goes where `|` is; returns the queries tried, in order.
function queries(text: string, lang = "ruby"): string[] | undefined {
  const at = text.indexOf("|");
  return buildLookup(text.replace("|", ""), at, lang)?.attempts.map((a) => a.query);
}

describe("buildLookup", () => {
  it("looks up a bare name", () => {
    assert.deepEqual(queries("  wid|get = 1"), ["widget"]);
  });

  it("drops a lowercase receiver, whose class is unknown", () => {
    assert.deepEqual(queries("widget.sa|ve"), ["save"]);
    assert.deepEqual(queries("widget.account.sa|ve"), ["save"]);
    assert.deepEqual(queries("find(1).sa|ve"), ["save"]);
  });

  it("scopes by a constant receiver, falling back to the bare name", () => {
    assert.deepEqual(queries("Account::Led|ger.open"), ["Account::Ledger", "Ledger"]);
    assert.deepEqual(queries("Account::Ledger.op|en"), ["Account::Ledger.open", "open"]);
    assert.deepEqual(queries("::Account::Led|ger"), ["Account::Ledger", "Ledger"]);
    assert.deepEqual(queries("Acc|ount::Ledger"), ["Account"]);
  });

  it("stops the scope at a lowercase dotted receiver", () => {
    assert.deepEqual(queries("ledger.Widget::Pa|rt"), ["Widget::Part", "Part"]);
  });

  it("asks for the constructor, then the class, on .new", () => {
    const lookup = buildLookup('x = Widget.new("a")', 12, "ruby");
    assert.deepEqual(lookup?.attempts, [
      { query: "Widget.new", names: ["initialize", "__init__", "constructor", "new", "Widget"] },
      { query: "Widget", names: ["Widget"] },
    ]);
  });

  it("strips self, this and @ivar receivers", () => {
    assert.deepEqual(queries("self.sa|ve"), ["save"]);
    assert.deepEqual(queries("@widget.sa|ve"), ["save"]);
    assert.deepEqual(queries("this.sa|ve()", "typescript"), ["save"]);
    assert.deepEqual(queries("Self::n|ew()", "rust"), ["new"]);
  });

  it("keeps a lowercase :: path but not crate/super", () => {
    assert.deepEqual(queries("crate::index::repo_r|oot(p)", "rust"), ["index::repo_root", "repo_root"]);
  });

  it("includes Ruby ?/! suffixes, wherever the cursor sits on them", () => {
    assert.deepEqual(queries("w.emp|ty?"), ["empty?"]);
    assert.deepEqual(queries("w.empty|?"), ["empty?"]);
    assert.deepEqual(queries("w.empty?|"), ["empty?"]);
    assert.deepEqual(queries("w.sa|ve!"), ["save!"]);
    assert.deepEqual(queries("a.cou|nt!=b"), ["count"]);
  });

  it("leaves ? alone outside Ruby", () => {
    assert.deepEqual(queries("x = a.emp|ty ? 1 : 2", "typescript"), ["empty"]);
    assert.deepEqual(queries("Widget?.bu|ild()", "typescript"), ["Widget.build", "build"]);
  });

  it("treats $ as part of a JS identifier but a Ruby global sigil", () => {
    assert.deepEqual(queries("$fe|tch()", "javascript"), ["$fetch"]);
    assert.equal(queries("$std|out"), undefined);
    assert.equal(queries("@na|me"), undefined);
  });

  it("underlines the whole scoped expression", () => {
    const lookup = buildLookup("x = Account::Ledger.open", 21, "ruby");
    assert.deepEqual([lookup?.start, lookup?.end], [4, 24]);
  });

  it("finds nothing on whitespace, punctuation or numbers", () => {
    assert.equal(queries("a  | b"), undefined);
    assert.equal(queries("(|)"), undefined);
    assert.equal(queries("x = 4|2"), undefined);
  });

  it("uses the word just before the cursor", () => {
    assert.deepEqual(queries("save| "), ["save"]);
  });
});

const hit = (name: string, confidence: number, file = "a.rb"): Hit => ({ name, kind: "method", file, line: 1, confidence });

describe("pickHits", () => {
  it("drops fuzzy and wrong-case matches", () => {
    assert.deepEqual(pickHits([hit("Widget", 1), hit("save_all", 0.9)], ["widget"]), []);
  });

  it("jumps straight to a lone exact match, however unsure rq is", () => {
    const picks = pickHits([hit("save_all", 0.6), hit("save", 0.4)], ["save"]);
    assert.deepEqual(picks.map((h) => h.name), ["save"]);
  });

  it("jumps straight to a confident top match", () => {
    const picks = pickHits([hit("save", 0.9, "a.rb"), hit("save", 0.1, "b.rb")], ["save"]);
    assert.deepEqual(picks.map((h) => h.file), ["a.rb"]);
  });

  it("offers a capped list when the matches are close", () => {
    const hits = Array.from({ length: 8 }, (_, i) => hit("save", 0.3, `${i}.rb`));
    assert.equal(pickHits(hits, ["save"]).length, 5);
  });
});

describe("parseResult", () => {
  it("reads hits on exit 0", () => {
    const res = parseResult(0, JSON.stringify([{ name: "save", kind: "method", file: "a.rb", line: 3, confidence: 1 }]));
    assert.equal(res.status, "ok");
    assert.equal(res.status === "ok" && res.hits[0].line, 3);
  });

  it("reports a miss on exit 1", () => {
    assert.deepEqual(parseResult(1, '{"query":"x","status":"no_match"}'), { status: "miss" });
  });

  it("reports warming on exit 2, with no hits from the status object", () => {
    assert.deepEqual(parseResult(2, '{"query":"x","status":"warming"}'), { status: "warming", hits: [] });
  });

  it("surfaces other failures with rq's stderr", () => {
    assert.deepEqual(parseResult(64, "", "error: bad flag\n"), { status: "error", message: "error: bad flag" });
    assert.equal(parseResult(0, "not json").status, "error");
  });

  it("reads exit 64 as a usage error, taking rq's JSON message", () => {
    const stdout = '{"error": "rq: unknown --kind \\"x\\"", "kind": "usage", "code": 64}';
    const res = parseResult(64, stdout, "rq: unknown --kind \"x\"\nUsage: rq ...\n");
    assert.deepEqual(res, { status: "error", message: 'rq: unknown --kind "x"' });
  });

  it("reads an older rq's exit-2 JSON error as an error, not a warming index", () => {
    const stdout = '{"error": "error: unexpected argument \'--anchor\' found", "kind": "usage", "code": 2}';
    const res = parseResult(2, stdout, "");
    assert.deepEqual(res, { status: "error", message: "error: unexpected argument '--anchor' found" });
  });

  it("reads exit 2 with only stderr as a usage error, not a warming index", () => {
    const res = parseResult(2, "", "error: unexpected argument '--anchor' found\n");
    assert.deepEqual(res, { status: "error", message: "error: unexpected argument '--anchor' found" });
  });

  it("skips rows without a location", () => {
    const res = parseResult(0, JSON.stringify([{ name: "x" }, { name: "y", file: "y.rb", line: 1, confidence: 1 }]));
    assert.equal(res.status === "ok" && res.hits.length, 1);
  });
});

describe("findRepoRoot", () => {
  it("returns the nearest ancestor with .git, like rq", () => {
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "rq-vscode-"));
    fs.mkdirSync(path.join(tmp, "repo", ".git"), { recursive: true });
    fs.mkdirSync(path.join(tmp, "repo", "lib", "deep"), { recursive: true });
    try {
      assert.equal(findRepoRoot(path.join(tmp, "repo", "lib", "deep"), fs.existsSync), path.join(tmp, "repo"));
      assert.equal(findRepoRoot(tmp, (p) => p === "/nowhere/.git"), undefined);
    } finally {
      fs.rmSync(tmp, { recursive: true, force: true });
    }
  });

  it("resolves hits against the root", () => {
    assert.equal(hitPath("/repo", hit("save", 1, "lib/a.rb")), path.resolve("/repo/lib/a.rb"));
  });

  it("prefers the root rq reports, e.g. another repo under -a", () => {
    const other = { ...hit("save", 1, "lib/a.rb"), root: "/elsewhere" };
    assert.equal(hitPath("/repo", other), path.resolve("/elsewhere/lib/a.rb"));
  });
});

describe("editorLanguages", () => {
  it("covers the JSX variants under typescript and javascript", () => {
    assert.deepEqual(editorLanguages(["ruby", "typescript", "javascript"]), [
      "ruby",
      "typescript",
      "typescriptreact",
      "javascript",
      "javascriptreact",
    ]);
  });
});

describe("unseen", () => {
  it("drops hits another provider already returned, by line or by name in the file", () => {
    const root = path.resolve("/repo");
    const a = path.join(root, "lib/a.rb");
    const taken = new Set([...symbolKeys(a, 1, "Ledger"), ...symbolKeys(path.join(root, "lib/b.rb"), undefined, "Widget")]);
    const hits = [hit("Ledger", 2, "lib/a.rb"), hit("Widget", 5, "lib/b.rb"), hit("save", 9, "lib/a.rb")];
    assert.deepEqual(unseen(root, hits, taken).map((h) => h.name), ["save"]);
  });
});

describe("rqLangs", () => {
  it("maps VS Code ids to rq's --lang, pairing TypeScript with JavaScript", () => {
    assert.equal(rqLangs(["ruby"]), "ruby");
    assert.equal(rqLangs(["typescriptreact"]), "typescript,javascript");
    assert.equal(rqLangs(["ruby", "go", "plaintext"]), "ruby,go");
  });
});
