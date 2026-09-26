import { execFile } from "node:child_process";
import { parseResult, RqResult } from "./query";

/** Longest rq may block on a cold index before answering with what's committed. */
const WAIT = "2s";
/** Hard stop for the child, past rq's own wait budget. */
const TIMEOUT_MS = 5000;

export type RunResult = RqResult | { status: "missing" } | { status: "cancelled" };

/**
 * Run `rq <query> --json` in `cwd` (rq resolves the repo from its cwd). The
 * signal kills the child — VS Code cancels a definition request as soon as the
 * cursor moves on.
 */
export function runRq(
  bin: string,
  query: string,
  extra: string[],
  cwd: string,
  signal?: AbortSignal,
): Promise<RunResult> {
  const args = ["--json", "--wait", WAIT, ...extra, "--", query];
  return new Promise((resolve) => {
    execFile(bin, args, { cwd, signal, timeout: TIMEOUT_MS, maxBuffer: 16 << 20 }, (err, stdout, stderr) => {
      if (!err) return resolve(parseResult(0, stdout, stderr));
      const e = err as NodeJS.ErrnoException & { code?: string | number; killed?: boolean };
      if (e.code === "ENOENT") return resolve({ status: "missing" });
      if (e.name === "AbortError" || signal?.aborted) return resolve({ status: "cancelled" });
      if (e.killed) return resolve({ status: "error", message: `rq timed out after ${TIMEOUT_MS} ms` });
      if (typeof e.code === "number") return resolve(parseResult(e.code, stdout, stderr));
      resolve({ status: "error", message: e.message });
    });
  });
}
