// Model driving: one real OpenCode process per task, consuming the
// computer-use MCP server exactly as a real user would (`computer-use-mcp`
// from PATH — installed from the release tarball, never linked to this
// workspace). OpenCode merges the project-level opencode.json (written
// here: just the MCP entry) with the user-level config
// (~/.config/opencode/opencode.json, which holds the model provider
// credentials). The provider key is never read, logged, or printed by the
// runner.
//
// The full OpenCode event stream is saved to the run dir (opencode.jsonl)
// for failure forensics; the runner only prints a per-task summary line.

import { spawn, spawnSync } from "node:child_process";
import { createWriteStream, writeFileSync } from "node:fs";
import { join } from "node:path";

/** The MCP server binary must be a real install (release tarball), not a
 * workspace path — the same honesty rule as the OpenCode acceptance. */
export function checkMcpBinary() {
  const r = spawnSync("sh", ["-c", "command -v computer-use-mcp"], { encoding: "utf8" });
  if (r.status !== 0) {
    throw new Error(
      "computer-use-mcp not found on PATH. Install the release tarball of " +
      "@computer-use/mcp-server (see benchmarks/README.md) — the benchmark " +
      "must not resolve the MCP server from this workspace."
    );
  }
  return r.stdout.trim();
}

/** Built-in tools the model must NOT get: the benchmark measures the
 * computer-use runtime, and a coding model with bash will happily script
 * the task (osascript/keystroke) without ever touching the MCP — real runs
 * did exactly that and reported PASS with 0 runtime actions. Block the
 * execution/scripting tools; read-only tools stay (harmless, and useful for
 * the model to inspect its environment). */
const BLOCKED_TOOLS = {
  bash: false,
  write: false,
  edit: false,
  patch: false,
  webfetch: false,
};

/** Write the opencode.json for a task run dir. Only the MCP entry plus the
 * tool restrictions are added; model/provider config stays in the user-level
 * config. */
export function writeOpenCodeConfig(runDir) {
  writeFileSync(
    join(runDir, "opencode.json"),
    JSON.stringify({
      tools: BLOCKED_TOOLS,
      mcp: {
        "computer-use": {
          type: "local",
          command: ["computer-use-mcp"],
          enabled: true,
        },
      },
    }, null, 2) + "\n",
  );
}

/**
 * Run one task through a real OpenCode process.
 *
 * @returns {{ exitCode: number|null, timedOut: boolean, durationMs: number,
 *             spawnError?: string, events: Array }}
 */
export function runOpenCode({ runDir, instruction, model, timeoutMs, onEvent }) {
  return new Promise((resolvePromise) => {
    const args = ["run", "--format", "json", "--auto", "--dir", runDir];
    if (model) args.push("--model", model);
    args.push("--", instruction);
    const child = spawn("opencode", args, { cwd: runDir, stdio: ["ignore", "pipe", "pipe"] });
    const started = Date.now();
    let timedOut = false;
    const events = [];
    let logStream;
    let errStream;
    try {
      logStream = createWriteStream(join(runDir, "opencode.jsonl"));
      // stderr is a first-class diagnostic (e.g. classifier outages, MCP
      // launch errors) — never discard it.
      errStream = createWriteStream(join(runDir, "opencode.stderr.log"));
    } catch { /* run dir should exist */ }

    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGTERM");
      // Escalate: a stuck model process must not wedge the benchmark.
      setTimeout(() => child.kill("SIGKILL"), 5000);
    }, timeoutMs);

    let buf = "";
    child.stdout.on("data", (chunk) => {
      buf += chunk.toString("utf8");
      let idx;
      while ((idx = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, idx);
        buf = buf.slice(idx + 1);
        if (!line.trim()) continue;
        try {
          const ev = JSON.parse(line);
          events.push(ev);
          logStream?.write(line + "\n");
          if (typeof onEvent === "function") onEvent(ev);
        } catch {
          logStream?.write(line + "\n");
        }
      }
    });
    child.stderr.on("data", (chunk) => errStream?.write(chunk));
    child.on("close", (code) => {
      clearTimeout(timer);
      try { logStream?.end(); } catch { /* ignore */ }
      try { errStream?.end(); } catch { /* ignore */ }
      resolvePromise({ exitCode: code, timedOut, durationMs: Date.now() - started, events });
    });
    child.on("error", (err) => {
      clearTimeout(timer);
      resolvePromise({ exitCode: null, timedOut, durationMs: Date.now() - started, spawnError: err.message, events });
    });
  });
}
