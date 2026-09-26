// Launches a real VS Code (downloaded by @vscode/test-electron) on a copy of
// the fixture, with the extension under development and an isolated rq index.
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { runTests } from "@vscode/test-electron";

async function main() {
  const ext = path.resolve(__dirname, "../../..");
  // A temp copy outside any git repo, so rq roots at the workspace folder
  // instead of indexing the rq checkout around the fixture.
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "rq-vscode-e2e-"));
  const workspace = path.join(tmp, "workspace");
  fs.cpSync(path.join(ext, "test", "fixture"), workspace, { recursive: true });

  try {
    await runTests({
      extensionDevelopmentPath: ext,
      extensionTestsPath: path.join(__dirname, "suite"),
      launchArgs: [workspace, "--disable-extensions", "--user-data-dir", path.join(tmp, "user")],
      extensionTestsEnv: { RQ_DB: path.join(tmp, "rq.db") },
    });
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
