import * as assert from "node:assert/strict";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { after, describe, it } from "node:test";
import { runRq } from "../../src/rq";

// A stand-in rq: echoes its args as a hit, or behaves per $STUB_MODE.
const dir = fs.mkdtempSync(path.join(os.tmpdir(), "rq-stub-"));
const stub = path.join(dir, "rq");
fs.writeFileSync(
  stub,
  `#!/bin/sh
case "$STUB_MODE" in
  miss) echo '{"status":"no_match"}'; exit 1 ;;
  hang) exec sleep 10 ;;
esac
printf '[{"name":"%s","kind":"method","file":"%s","line":1,"confidence":1}]' "$*" "$PWD"
`,
  { mode: 0o755 },
);
after(() => fs.rmSync(dir, { recursive: true, force: true }));

describe("runRq", () => {
  it("passes flags before the query, in the given cwd", async () => {
    const res = await runRq(stub, "-weird", ["-l", "3"], dir);
    assert.equal(res.status, "ok");
    const [h] = res.status === "ok" ? res.hits : [];
    assert.equal(h.name, "--json --wait 2s -l 3 -- -weird");
    assert.equal(fs.realpathSync(h.file), fs.realpathSync(dir));
  });

  it("maps exit 1 to a miss", async () => {
    process.env.STUB_MODE = "miss";
    try {
      assert.deepEqual(await runRq(stub, "x", [], dir), { status: "miss" });
    } finally {
      delete process.env.STUB_MODE;
    }
  });

  it("reports a missing binary distinctly", async () => {
    assert.deepEqual(await runRq(path.join(dir, "nope"), "x", [], dir), { status: "missing" });
  });

  it("kills the child on cancel", async () => {
    process.env.STUB_MODE = "hang";
    try {
      const abort = new AbortController();
      const started = Date.now();
      const pending = runRq(stub, "x", [], dir, abort.signal);
      setTimeout(() => abort.abort(), 50);
      assert.deepEqual(await pending, { status: "cancelled" });
      assert.ok(Date.now() - started < 2000);
    } finally {
      delete process.env.STUB_MODE;
    }
  });
});
