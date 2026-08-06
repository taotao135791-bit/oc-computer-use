// Run directory management. Every task gets a scratch dir under the run's
// root; the {{SCRATCH}} placeholder in task instructions resolves there.
// Task-relative scripts (initial_state / cleanup_script) run with cwd =
// benchmarks/ so their relative paths stay stable across run dirs.

import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

export function createRunDir(runId) {
  const dir = resolve(`/tmp/cu-bench-${runId}`);
  mkdirSync(dir, { recursive: true });
  return dir;
}

export function taskScratchDir(runDir, taskId) {
  const dir = join(runDir, "scratch", taskId);
  mkdirSync(dir, { recursive: true });
  return dir;
}

/** Pre-create files the task needs (declared as initial content), e.g. a
 * document the model must open. */
export function seedFile(path, content) {
  mkdirSync(resolve(path, ".."), { recursive: true });
  writeFileSync(path, content);
}

/** Run a task-relative shell script (initial_state / cleanup_script). */
export function runScript(scriptPath, { taskId, env }) {
  const r = spawnSync("/bin/bash", [scriptPath], {
    encoding: "utf8",
    cwd: resolve(scriptPath, "..", ".."),
    env: { ...process.env, ...env },
  });
  if (r.status !== 0) {
    throw new Error(`task ${taskId}: script ${scriptPath} failed (exit ${r.status}): ${(r.stderr || r.stdout || "").slice(0, 500)}`);
  }
  return r;
}
