#!/usr/bin/env node
// cu-bench — the Computer Use Benchmark runner.
//
//   cu-bench list [--suite smoke|full] [--category <c>] [--json]
//   cu-bench run --suite smoke|full [--tasks id1,id2] [--host opencode|pi]
//                [--model provider/model] [--run-id <id>] [--keep-files]
//   cu-bench replay <run-dir>   (re-runs nothing; re-evaluates criteria
//                                against current system state — only valid
//                                for tasks whose state still matches)
//   cu-bench report <run-dir> [--out <reports-dir>]
//   cu-bench compare <runA-dir> <runB-dir>
//
// Design rules (benchmark spec):
//  - The runner never plans: it hands the instruction to a real model host
//    (OpenCode by default) and judges ONLY via the evaluator.
//  - No per-task coordinates, no task-id-driven scripts, no relaxing of
//    criteria, no deleting of failed tasks, no hand-editing of results.
//  - Tokens are never printed; traces are read with the observation token
//    from the CLI-persisted credential (never a bare session id).
//  - Screenshots are never committed; run data lives under /tmp/cu-bench-*.

import { spawn, spawnSync } from "node:child_process";
import { appendFileSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { loadTasks, resolveTask, taskScriptPath } from "./lib/tasks.mjs";
import { evaluateTask } from "./lib/evaluate.mjs";
import { checkMcpBinary, runOpenCode, writeOpenCodeConfig } from "./lib/model.mjs";
import { summarizeTrace, classifyFailure, failureDetail, readTrace, latestCredential, cleanStaleSessions } from "./lib/trace.mjs";
import { createRunDir, taskScratchDir, runScript } from "./lib/scratch.mjs";
import { generateReport, compareRuns } from "./lib/report.mjs";

const SMOKE_TASKS = [
  "textedit-01-create-and-save",
  "textedit-02-open-append-save",
  "finder-01-create-folder",
  "finder-02-rename-file",
  "settings-01-appearance-toggle",
  "calc-01-simple-addition",
  "safari-01-open-fixture",
  "safari-03-fill-form",
  "cross-01-textedit-to-web",
  "cross-05-copy-paste-across-apps",
];

const FIXTURE_PORT = 8931;

function usage() {
  console.log(`usage:
  cu-bench list [--suite smoke|full] [--category <c>] [--json]
  cu-bench run --suite smoke|full [--tasks id1,id2] [--host opencode|pi] [--model <m>] [--run-id <id>] [--keep-files] [--fixture-port <p>]
  cu-bench report <run-dir> [--out <reports-dir>]
  cu-bench compare <runA-dir> <runB-dir>
  cu-bench replay <run-dir>`);
  process.exit(2);
}

function arg(name, def) {
  const i = process.argv.indexOf(`--${name}`);
  if (i >= 0 && process.argv[i + 1] && !process.argv[i + 1].startsWith("--")) return process.argv[i + 1];
  return def;
}
function flag(name) {
  return process.argv.includes(`--${name}`);
}

// The fixture web app runs as its own process (single implementation in
// benchmarks/fixtures/webapp/server.mjs); the runner only manages its
// lifecycle and talks to it over real HTTP.
const FIXTURE_SERVER = resolve(import.meta.dirname, "..", "fixtures", "webapp", "server.mjs");
let fixtureProcess = null;
async function startFixture(port) {
  fixtureProcess = spawn(process.execPath, [FIXTURE_SERVER], {
    env: { ...process.env, PORT: String(port) },
    stdio: ["ignore", "ignore", "inherit"],
  });
  // Wait for the server to answer before declaring it up.
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    try { await fetch(`http://127.0.0.1:${port}/reset`); return; }
    catch { await new Promise((r) => setTimeout(r, 200)); }
  }
  throw new Error(`fixture web app did not start on port ${port}`);
}
function stopFixture() {
  try { fixtureProcess?.kill("SIGTERM"); } catch { /* ignore */ }
}

async function environment() {
  const run = (cmd, args) => { const r = spawnSync(cmd, args, { encoding: "utf8" }); return r.status === 0 ? r.stdout.trim() : "unavailable"; };
  return {
    macos: run("sw_vers", ["-productVersion"]),
    arch: run("uname", ["-m"]),
    commit: run("git", ["rev-parse", "--short", "HEAD"]),
    runtime_version: run("cu", ["--version"]).split("\n")[0] || "unavailable",
    protocol_version: (() => { try { return JSON.parse(readFileSync(resolve(import.meta.dirname, "../../protocol/computer-use.schema.json"), "utf8"))["x-protocol-meta"]?.protocol_version; } catch { return null; } })(),
    daemon_health: (() => { try { const r = spawnSync("cu", ["daemon", "status"], { encoding: "utf8" }); return r.status === 0 ? r.stdout.trim().slice(0, 80) : "not running"; } catch { return "unavailable"; } })(),
    fixture_port: FIXTURE_PORT,
  };
}

async function main() {
  const cmd = process.argv[2];
  if (!cmd) return usage();
  switch (cmd) {
    case "list": return cmdList();
    case "run": return cmdRun();
    case "report": return cmdReport();
    case "compare": return cmdCompare();
    case "replay": return cmdReplay();
    default: return usage();
  }
}

function selectTasks(suite, taskFilter) {
  const all = loadTasks();
  const want = new Set(taskFilter ? taskFilter.split(",").map((s) => s.trim()).filter(Boolean) : []);
  let sel = all;
  if (want.size) sel = all.filter((t) => want.has(t.id));
  else if (suite === "smoke") sel = all.filter((t) => SMOKE_TASKS.includes(t.id));
  else if (suite === "full") sel = all;
  else throw new Error(`unknown suite '${suite}' (smoke|full)`);
  if (!sel.length) throw new Error(`no tasks selected (suite=${suite}, tasks=${taskFilter || "all"})`);
  return sel;
}

function cmdList() {
  const suite = arg("suite", "full");
  const category = arg("category", null);
  const all = loadTasks();
  const sel = suite === "smoke" ? all.filter((t) => SMOKE_TASKS.includes(t.id)) : all;
  const filtered = category ? sel.filter((t) => t.category === category) : sel;
  if (flag("json")) {
    console.log(JSON.stringify(filtered.map(({ _file, ...t }) => ({ id: t.id, name: t.name, category: t.category, difficulty: t.difficulty, risk_level: t.risk_level })), null, 2));
    return;
  }
  const byCat = {};
  for (const t of filtered) (byCat[t.category] ||= []).push(t.id);
  for (const [cat, ids] of Object.entries(byCat)) console.log(`${cat} (${ids.length}):\n  ${ids.join("\n  ")}`);
  console.log(`\n${filtered.length} tasks`);
}

async function cmdRun() {
  const suite = arg("suite", null);
  const taskFilter = arg("tasks", null);
  if (!suite && !taskFilter) throw new Error("run requires --suite smoke|full or --tasks <ids>");
  const host = arg("host", "opencode");
  const model = arg("model", null);
  const runId = arg("run-id", null) || `${new Date().toISOString().slice(0, 10)}-${host}-${Date.now().toString(36)}`;
  const keepFiles = flag("keep-files");
  const port = Number(arg("fixture-port", String(FIXTURE_PORT)));

  if (host === "opencode") checkMcpBinary();
  if (host !== "opencode" && host !== "pi") throw new Error(`unknown host '${host}' (opencode|pi)`);

  const tasks = selectTasks(suite, taskFilter);
  const runDir = createRunDir(runId);
  const resultsFile = join(runDir, "task-results.jsonl");
  const env = await environment();
  writeFileSync(join(runDir, "environment.json"), JSON.stringify(env, null, 2) + "\n");
  console.log(`run ${runId}: ${tasks.length} tasks, host=${host}, dir=${runDir}`);
  console.log(`environment: ${env.macos} ${env.arch}; ${env.daemon_health}`);

  let fixtureUp = false;
  if (tasks.some((t) => t.environment?.fixture_required)) {
    await startFixture(port);
    fixtureUp = true;
    console.log(`fixture web app on http://127.0.0.1:${port}`);
  }
  const fixtureUrl = `http://127.0.0.1:${port}`;

  let pass = 0;
  for (const task of tasks) {
    const t0 = Date.now();
    const startedAt = new Date().toISOString();
    const scratchDir = taskScratchDir(runDir, task.id);
    const resolved = resolveTask(task, { scratchDir, fixtureUrl });
    const line = `\n=== ${task.id} (${task.category}/${task.difficulty}) — ${task.name}`;
    console.log(line);

    // A session left behind by an earlier client (SIGKILLed MCP server, old
    // CLI one-shot) holds the daemon's control lock and would fail this
    // task's `session start` with CONTROL_LOCKED — observed in round 7.
    // Stop every session the runner has a credential for (non-fatal).
    try {
      await cleanStaleSessions({ log: (m) => console.log(`  ${m}`) });
    } catch (e) {
      console.log(`  (stale-session cleanup failed: ${e.message})`);
    }

    try {
      if (fixtureUp) {
        try { await fetch(`${fixtureUrl}/reset`); } catch { /* server just started */ }
      }
      const init = taskScriptPath(task, "initial_state");
      if (init) runScript(init, { taskId: task.id, env: { SCRATCH: scratchDir, FIXTURE_URL: fixtureUrl } });

      writeOpenCodeConfig(scratchDir);
      const result = await runOpenCode({
        runDir: scratchDir,
        instruction: resolved.instruction,
        model,
        timeoutMs: (task.max_duration_seconds + 60) * 1000,
      });

      // Session trace: latest CLI-persisted credential (the MCP server
      // starts the session through the daemon; the CLI credential file is
      // the same-UID, 0600 record of it).
      const cred = latestCredential();
      let traceEntries = null;
      let traceId = cred?.sid ?? null;
      if (cred) {
        try { traceEntries = await readTrace(cred.sid); }
        catch (e) { console.log(`  (trace read failed: ${e.message})`); }
      }

      const ev = await evaluateTask(resolved, { fixtureUrl });
      const success = ev.pass;
      const metrics = summarizeTrace(traceEntries || []);
      const timedOut = result.timedOut;
      const category = classifyFailure({ success, entries: traceEntries || [], runTimedOut: timedOut });
      const detail = !success ? (timedOut
        ? `model run exceeded max_duration_seconds (${task.max_duration_seconds}s)`
        : failureDetail(traceEntries || [], false))
        : category === "RUNTIME_NOT_DRIVEN"
          ? `evaluator passed but runtime was not driven (observe_calls=${metrics.observe_calls}, total_actions=${metrics.total_actions}); gated — not counted as a runtime success`
          : "";

      const record = {
        run_id: runId,
        task_id: task.id,
        task_category: task.category,
        task_difficulty: task.difficulty,
        harness: host,
        model: model ?? "default",
        started_at: startedAt,
        completed_at: new Date().toISOString(),
        success,
        partial_success: ev.humanReview && success,
        ...metrics,
        duration_ms: result.durationMs,
        trace_id: traceId,
        failure_category: category,
        failure_detail: detail,
        human_review_required: ev.humanReview,
      };
      appendFileSync(resultsFile, JSON.stringify(record) + "\n");
      // RUNTIME_NOT_DRIVEN: evaluator passed but the runtime was never
      // driven — the model reached the goal without the runtime (or by
      // hallucination). Gated: it is NOT counted as a runtime success.
      const gated = success && category === "RUNTIME_NOT_DRIVEN";
      const verdict = gated ? "GATED" : success ? "PASS" : "FAIL";
      const hr = ev.humanReview ? " (human review pending)" : "";
      console.log(`  ${verdict}${hr} ${result.durationMs}ms ${metrics.total_actions} actions, trace=${traceId || "none"}, ${category}`);
      if (detail) console.log(`  detail: ${detail}`);
      if (success && !gated) pass++;
    } catch (e) {
      const record = {
        run_id: runId, task_id: task.id, task_category: task.category, task_difficulty: task.difficulty,
        harness: host, model: model ?? "default", started_at: startedAt, completed_at: new Date().toISOString(),
        success: false, partial_success: false,
        observe_calls: 0, inspect_calls: 0, action_batches: 0, total_actions: 0,
        stale_frame_count: 0, cancelled_request_count: 0, timeout_count: 0, recovery_count: 0,
        user_takeover_count: 0, duration_ms: Date.now() - t0, screenshot_bytes: 0, trace_id: null,
        failure_category: "HARNESS_INTEGRATION_ERROR", failure_detail: e.message.slice(0, 500),
        human_review_required: false,
      };
      appendFileSync(resultsFile, JSON.stringify(record) + "\n");
      console.log(`  FAIL harness: ${e.message.slice(0, 300)}`);
    }

    const cleanup = taskScriptPath(task, "cleanup_script");
    if (cleanup && !keepFiles) {
      try { runScript(cleanup, { taskId: task.id, env: { SCRATCH: scratchDir } }); }
      catch (e) { console.log(`  (cleanup failed: ${e.message})`); }
    }
  }

  stopFixture();
  console.log(`\nrun ${runId}: ${pass}/${tasks.length} passed. Report: cu-bench report ${runDir} [--out benchmarks/reports/...]`);
  if (keepFiles) console.log(`(run dir kept: ${runDir})`);
  process.exit(pass === tasks.length ? 0 : 1);
}

function cmdReport() {
  const runDir = resolve(arg("report", process.argv[3] || ""));
  if (!runDir || !existsSync(join(runDir, "task-results.jsonl"))) {
    console.error(`no task-results.jsonl in ${runDir}`);
    process.exit(1);
  }
  const results = readFileSync(join(runDir, "task-results.jsonl"), "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l));
  const env = existsSync(join(runDir, "environment.json")) ? JSON.parse(readFileSync(join(runDir, "environment.json"), "utf8")) : {};
  const out = arg("out", null) || join(runDir, "report");
  const runId = results[0]?.run_id || runDir.split("/").pop();
  const harness = results[0]?.harness || "unknown";
  const model = results[0]?.model || null;
  generateReport(out, { results, environment: env, runId, harness, model });
  console.log(`report written to ${out}/ (${results.length} results)`);
}

function cmdCompare() {
  const a = resolve(process.argv[3] || "");
  const b = resolve(process.argv[4] || "");
  const sa = join(a, "report", "summary.json");
  const sb = join(b, "report", "summary.json");
  if (!existsSync(sa) || !existsSync(sb)) {
    console.error("usage: cu-bench compare <runA-dir> <runB-dir> (each with report/summary.json)");
    process.exit(1);
  }
  console.log(compareRuns(sa, sb));
}

function cmdReplay() {
  console.log("replay: re-runs nothing. Use 'report' for the recorded run; re-evaluating criteria against the current desktop is only valid when the task state still matches (run the original task again with --tasks <id> instead).");
  process.exit(0);
}

main().catch((e) => {
  console.error(`cu-bench: ${e.message}`);
  process.exit(1);
});
