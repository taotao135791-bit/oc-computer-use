// Regression tests for the round-7 success gate (Fix 9):
// a task whose evaluator passed but whose runtime was never driven
// (RUNTIME_NOT_DRIVEN) must NOT be counted as a runtime success — neither by
// classifyFailure nor by the report aggregation.
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { classifyFailure } from "../lib/trace.mjs";
import { generateReport } from "../lib/report.mjs";

test("classifyFailure: evaluator success with 0 actions is RUNTIME_NOT_DRIVEN", () => {
  // calc-01 round-1 shape: 1 observe, 0 actions, evaluator passed.
  const entries = [
    { event: "session.start", session_id: "s_test" },
    { event: "observe", result: { status: "ok" } },
  ];
  assert.equal(classifyFailure({ success: true, entries, runTimedOut: false }), "RUNTIME_NOT_DRIVEN");
});

test("classifyFailure: evaluator success with real driving is SUCCESS", () => {
  const entries = [
    { event: "session.start", session_id: "s_test" },
    { event: "observe", result: { status: "ok" } },
    { event: "action", action: { type: "key" }, result: { status: "ok" } },
  ];
  assert.equal(classifyFailure({ success: true, entries, runTimedOut: false }), "SUCCESS");
});

test("report: RUNTIME_NOT_DRIVEN is excluded from success count and listed as failure", () => {
  const dir = mkdtempSync(join(tmpdir(), "cu-bench-gate-"));
  try {
    const base = {
      run_id: "t", task_category: "calc", task_difficulty: "easy", harness: "opencode",
      model: "x", partial_success: false, human_review_required: true,
      observe_calls: 1, inspect_calls: 0, action_batches: 0, total_actions: 0,
      stale_frame_count: 0, cancelled_request_count: 0, timeout_count: 0,
      recovery_count: 0, user_takeover_count: 0, duration_ms: 30000,
      screenshot_bytes: 0, failure_detail: "",
    };
    const results = [
      { ...base, task_id: "calc-gated", success: true, failure_category: "RUNTIME_NOT_DRIVEN", trace_id: "s_g" },
      { ...base, task_id: "calc-real", success: true, failure_category: "SUCCESS", trace_id: "s_r" },
      { ...base, task_id: "calc-fail", success: false, failure_category: "MODEL_PLANNING_ERROR", trace_id: "s_f" },
    ];
    generateReport(dir, { results, environment: {}, runId: "t", harness: "opencode", model: "x" });
    const summary = JSON.parse(readFileSync(join(dir, "summary.json"), "utf8"));
    assert.equal(summary.success_count, 1, "gated task must not count as success");
    assert.equal(summary.success_rate_pct, 33.3);
    assert.equal(summary.model_failures, 2, "RUNTIME_NOT_DRIVEN counts as model failure");
    const failures = readFileSync(join(dir, "failures.md"), "utf8");
    assert.match(failures, /calc-gated — RUNTIME_NOT_DRIVEN/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
