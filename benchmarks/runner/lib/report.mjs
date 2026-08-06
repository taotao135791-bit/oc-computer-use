// Report generation from a run's task-results.jsonl. Emits the report files
// mandated by the benchmark spec (benchmarks/reports/<date>-alpha2/):
//   summary.md, summary.json, failures.md, metrics.csv, environment.json,
//   opencode-results.md (+ pi-results.md / comparison.md stubs with honest
//   NOT VERIFIED / NOT RUN status until those acceptances exist).
//
// Numbers are computed from the results file only — nothing is hand-edited.

import { readFileSync, writeFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";

const RUNTIME_FAILURES = new Set([
  "GROUNDING_MISS", "SMALL_TARGET_MISS", "STALE_FRAME_RECOVERY_FAILED",
  "INPUT_FOCUS_LOST", "TEXT_INPUT_FAILED", "UNICODE_INPUT_FAILED",
  "SCROLL_DIRECTION_ERROR", "DRAG_FAILED", "WINDOW_SWITCH_ERROR",
  "APPLICATION_NOT_FOUND", "CANCEL_FAILED", "SESSION_ERROR",
  "PERMISSION_ERROR", "HARNESS_INTEGRATION_ERROR", "ACTION_TIMEOUT",
  "STABILIZER_TIMEOUT", "STATE_DETECTION_ERROR",
]);
const MODEL_FAILURES = new Set([
  "MODEL_PLANNING_ERROR", "MODEL_STOPPED_EARLY", "RUNTIME_NOT_DRIVEN",
]);
const VALIDATION_FAILURES = new Set(["SUCCESS_VALIDATION_ERROR"]);

function csvEscape(v) {
  const s = String(v ?? "");
  return /[",\n]/.test(s) ? `"${s.replaceAll('"', '""')}"` : s;
}

export function generateReport(reportDir, { results, environment, runId, harness, model }) {
  mkdirSync(reportDir, { recursive: true });

  const lines = results.map((r) => JSON.stringify(r)).join("\n") + "\n";
  writeFileSync(join(reportDir, "task-results.jsonl"), lines);

  const n = results.length;
  // Success is gated by failure_category: RUNTIME_NOT_DRIVEN (evaluator
  // passed but the runtime was never driven) is NOT a runtime success and is
  // counted as a model failure, never as a pass.
  const isSuccess = (r) => r.success && r.failure_category !== "RUNTIME_NOT_DRIVEN";
  const success = results.filter(isSuccess);
  const humanReview = results.filter((r) => r.human_review_required);
  const failures = results.filter((r) => !isSuccess(r));
  const successRate = n ? (success.length / n) * 100 : 0;
  const auto = results.filter((r) => !r.human_review_required);
  const autoSuccessRate = auto.length ? (auto.filter(isSuccess).length / auto.length) * 100 : 0;

  const avg = (f) => {
    const vals = results.map(f).filter((v) => typeof v === "number" && v >= 0);
    return vals.length ? vals.reduce((a, b) => a + b, 0) / vals.length : null;
  };

  const counts = {};
  for (const r of failures) counts[r.failure_category] = (counts[r.failure_category] || 0) + 1;
  const taxonomy = Object.entries(counts).sort((a, b) => b[1] - a[1]);
  const runtimeCount = failures.filter((r) => RUNTIME_FAILURES.has(r.failure_category)).length;
  const modelCount = failures.filter((r) => MODEL_FAILURES.has(r.failure_category)).length;

  // summary.json
  const summary = {
    run_id: runId,
    harness,
    model: model ?? null,
    environment,
    task_count: n,
    success_count: success.length,
    success_rate_pct: +successRate.toFixed(1),
    auto_verified: { tasks: auto.length, success: auto.filter((r) => r.success).length, success_rate_pct: +autoSuccessRate.toFixed(1) },
    human_review_pending: humanReview.length,
    avg_steps: avg((r) => r.total_steps),
    avg_duration_ms: avg((r) => r.duration_ms),
    avg_actions: avg((r) => r.total_actions),
    avg_screenshot_bytes: avg((r) => r.screenshot_bytes),
    stale_frame_count_total: results.reduce((s, r) => s + r.stale_frame_count, 0),
    cancelled_request_total: results.reduce((s, r) => s + r.cancelled_request_count, 0),
    user_takeover_total: results.reduce((s, r) => s + r.user_takeover_count, 0),
    recovery_total: results.reduce((s, r) => s + r.recovery_count, 0),
    runtime_failures: runtimeCount,
    model_failures: modelCount,
    failure_taxonomy: Object.fromEntries(taxonomy),
    generated_at: new Date().toISOString(),
  };
  writeFileSync(join(reportDir, "summary.json"), JSON.stringify(summary, null, 2) + "\n");

  // environment.json
  writeFileSync(join(reportDir, "environment.json"), JSON.stringify(environment, null, 2) + "\n");

  // metrics.csv
  const header = ["task_id", "category", "difficulty", "success", "partial_success", "human_review", "duration_ms", "total_steps", "observe_calls", "inspect_calls", "action_batches", "total_actions", "stale_frame_count", "cancelled_request_count", "timeout_count", "recovery_count", "user_takeover_count", "screenshot_bytes", "failure_category"];
  const rows = results.map((r) => [
    r.task_id, r.task_category ?? "", r.task_difficulty ?? "", r.success, r.partial_success,
    r.human_review_required, r.duration_ms, r.total_steps, r.observe_calls, r.inspect_calls,
    r.action_batches, r.total_actions, r.stale_frame_count, r.cancelled_request_count,
    r.timeout_count, r.recovery_count, r.user_takeover_count, r.screenshot_bytes, r.failure_category,
  ].map(csvEscape).join(","));
  writeFileSync(join(reportDir, "metrics.csv"), header.join(",") + "\n" + rows.join("\n") + "\n");

  // failures.md
  const failLines = failures.map((r) => {
    const lines = [`## ${r.task_id} — ${r.failure_category}`, "", `- trace: \`${r.trace_id}\``,
      `- detail: ${r.failure_detail || "none"}`, ""];
    return lines.join("\n");
  }).join("\n");
  writeFileSync(join(reportDir, "failures.md"), failures.length
    ? `# Failures (${failures.length})\n\n${failLines}`
    : "# Failures\n\nNone.\n");

  // opencode-results.md
  const host = results.filter((r) => r.harness === "opencode");
  const hostSuccess = host.filter(isSuccess).length;
  writeFileSync(join(reportDir, "opencode-results.md"), `# OpenCode Host Results — ${runId}

- Harness: real \`opencode run\` (v1.x) consuming \`computer-use-mcp\` from PATH
  (release tarball install, not workspace source); model: ${model || "default from user config"}
- Tasks: ${host.length}, success: ${hostSuccess} (${host.length ? ((hostSuccess / host.length) * 100).toFixed(1) : 0}%)
- Image rendering: computer_observe screenshots (see metrics.csv screenshot_bytes column)
- Model tool use: ${host.some((r) => r.total_actions > 0) ? "observed (see task-results.jsonl)" : "none observed"}

Per-task details are in task-results.jsonl and failures.md.
`);

  // Honest stubs for acceptances not yet performed (no faking).
  writeFileSync(join(reportDir, "pi-results.md"), `# Pi Host Acceptance — ${runId}

Status: **NOT VERIFIED** in this run (no real Pi host session was driven).
Performed with a real Pi host (never a shim) and reported here when done.
`);

  writeFileSync(join(reportDir, "comparison.md"), `# Cross-runtime Comparison — ${runId}

Status: **NOT RUN** — competitors (open-codex-computer-use, Cua Computer
Drivers, a plain PyAutoGUI driver) were not executed in this run. When they
are, this file records per-runtime success rate / avg duration / avg steps /
mouse-stealing behavior / stale-frame protection / cancel capability / trace
capability / install complexity / multi-client isolation under as-equal-as-
possible conditions. A competitor that cannot run is marked NOT RUN, never
silently dropped.
`);

  // summary.md
  const failTop = taxonomy.slice(0, 5).map(([k, v]) => `- ${k}: ${v}`).join("\n") || "- none";
  const md = `# Benchmark Summary — ${runId}

- **Harness**: ${harness}; model: ${model || "default"}
- **Environment**: ${environment.macos} ${environment.arch}, runtime ${environment.runtime_version}, protocol v${environment.protocol_version}
- **Commit**: ${environment.commit || "unknown"}; daemon: ${environment.daemon_version || "n/a"}
- **Tasks**: ${n} total; **success**: ${success.length} (${successRate.toFixed(1)}%)
  - auto-verified: ${auto.length} tasks, ${autoSuccessRate.toFixed(1)}% success
  - human-review pending: ${humanReview.length} (calculator-style tasks; verdicts recorded separately)
- **Avg steps**: ${fmt(avg((r) => r.total_steps))}; **avg duration**: ${fmt(avg((r) => r.duration_ms))} ms; **avg actions**: ${fmt(avg((r) => r.total_actions))}
- **Stale frames**: ${summary.stale_frame_count_total}; **cancelled**: ${summary.cancelled_request_total}; **user takeovers**: ${summary.user_takeover_total}; **recoveries**: ${summary.recovery_total}
- **Avg screenshot bytes/task**: ${fmt(avg((r) => r.screenshot_bytes))}

## Failure taxonomy (top 5)

${failTop}

- **Runtime failures**: ${runtimeCount} — **model failures**: ${modelCount}
- Metrics notes: \`inspect_calls\` and \`recovery_count\` are 0 because the
  runtime records no inspect/recovery trace events yet (honest 0, not
  measured); stale-frame count comes from \`act.stale_rejected\` trace events
  (added this round).

## Known limitations

- ${humanReview.length} task(s) need human verdicts (see failures.md / task-results.jsonl for trace ids).
- Pi host and competitor-comparison sections are NOT VERIFIED / NOT RUN (see pi-results.md, comparison.md).
`;
  writeFileSync(join(reportDir, "summary.md"), md);
}

function fmt(v) {
  return v === null || v === undefined ? "n/a" : Number.isInteger(v) ? String(v) : v.toFixed(1);
}

/** Compare two runs' summaries. */
export function compareRuns(aPath, bPath) {
  const a = JSON.parse(readFileSync(aPath, "utf8"));
  const b = JSON.parse(readFileSync(bPath, "utf8"));
  return `# Compare: ${a.run_id} → ${b.run_id}

| metric | ${a.run_id} | ${b.run_id} |
|---|---|---|
| tasks | ${a.task_count} | ${b.task_count} |
| success rate | ${a.success_rate_pct}% | ${b.success_rate_pct}% |
| avg steps | ${fmt(a.avg_steps)} | ${fmt(b.avg_steps)} |
| avg duration ms | ${fmt(a.avg_duration_ms)} | ${fmt(b.avg_duration_ms)} |
| stale frames | ${a.stale_frame_count_total} | ${b.stale_frame_count_total} |
| runtime failures | ${a.runtime_failures} | ${b.runtime_failures} |
| model failures | ${a.model_failures} | ${b.model_failures} |
`;
}
