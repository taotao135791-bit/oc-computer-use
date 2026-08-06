// Trace reading and metric extraction.
//
// Session traces are a sensitive read: reading one requires the session's
// observation token. The runner reads the credential file the CLI persisted
// (`~/.local/state/oc-computer-use/credentials/<sid>.json`, mode 0600,
// same-UID) and calls `trace.get` through the SDK with that token. Tokens
// never appear in runner output, logs, or reports.

import { existsSync, readdirSync, readFileSync, rmSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { ComputerUseClient, defaultSocketPath } from "@computer-use/sdk";

export function credentialsDir() {
  return join(homedir(), ".local", "state", "oc-computer-use", "credentials");
}

/** The session credential file most recently created by the CLI (or null). */
export function latestCredential() {
  const dir = credentialsDir();
  if (!existsSync(dir)) return null;
  const files = readdirSync(dir)
    .filter((f) => f.endsWith(".json"))
    .map((f) => ({ f, p: join(dir, f) }))
    .sort((a, b) => statSync(b.p).mtimeMs - statSync(a.p).mtimeMs);
  if (!files.length) return null;
  return { sid: files[0].f.replace(/\.json$/, ""), path: files[0].p };
}

/** Stop every session with a persisted credential. A session whose owning
 * client exited without stopping it (a SIGKILLed MCP server, an old CLI
 * one-shot) keeps the daemon's control lock, and the next task's `session
 * start` fails with CONTROL_LOCKED — a real round-7 failure observed via
 * trace. This runner holds each session's control token (the same-UID 0600
 * credential file the daemon issued at start), so it can stop exactly the
 * sessions it has credentials for; sessions without a record cannot be
 * stopped (the daemon refuses a stop without the owner token) and are
 * reported through `log`. Returns the number of sessions stopped. */
export async function cleanStaleSessions({ log = () => {} } = {}) {
  const dir = credentialsDir();
  if (!existsSync(dir)) return 0;
  const files = readdirSync(dir).filter((f) => f.endsWith(".json"));
  let stopped = 0;
  let dropped = 0;
  for (const f of files) {
    const sid = f.replace(/\.json$/, "");
    let cred;
    try {
      cred = JSON.parse(readFileSync(join(dir, f), "utf8"));
    } catch {
      continue;
    }
    if (!cred.control_token) continue;
    const client = new ComputerUseClient({ socketPath: defaultSocketPath() });
    try {
      await client.connect();
      client.setSessionCredential({
        sessionId: sid,
        controlToken: cred.control_token,
        access: "control",
      });
      const res = await client.session("stop", { session_id: sid });
      if (res?.state) stopped++;
    } catch (e) {
      // Session already gone (or daemon down): its token died with it — drop
      // the record so a later `latestCredential()` never picks a dead
      // session. A stopped session's trace remains readable on disk.
      if (/SESSION_NOT_FOUND|session not found/i.test(String(e?.message ?? e))) {
        rmSync(join(dir, f), { force: true });
        dropped++;
      }
    } finally {
      client.close();
    }
  }
  if (stopped || dropped) {
    log(`stale sessions: ${stopped} stopped, ${dropped} dead credential records dropped`);
  }
  return stopped;
}

/** Read a session's trace entries through the SDK, using the observation
 * token from the CLI-persisted credential. Returns null when no credential
 * exists (e.g. the session was created without the CLI). */
export async function readTrace(sid) {
  const credPath = join(credentialsDir(), `${sid}.json`);
  if (!existsSync(credPath)) return null;
  const cred = JSON.parse(readFileSync(credPath, "utf8"));
  const client = new ComputerUseClient({ socketPath: defaultSocketPath() });
  try {
    await client.connect();
    // The observation token (from the CLI-persisted credential, never
    // printed) authorizes the trace read — a session id alone grants nothing.
    client.setSessionCredential({
      sessionId: sid,
      observationToken: cred.observation_token,
      access: "read_only",
    });
    return await client.traceGet(sid);
  } finally {
    client.close();
  }
}

/** Metrics derived from trace entries. Fields whose events the runtime does
 * not (yet) record are 0 with an explanatory note — never guessed. */
export function summarizeTrace(entries) {
  const actions = entries.filter((e) => e.event === "action");
  const observes = entries.filter((e) => e.event === "observe");
  const stale = entries.filter((e) => e.event === "act.stale_rejected");
  const cancels = entries.filter((e) => e.event === "cancel");
  const takeovers = entries.filter((e) => e.event === "session.takeover");
  const batches = new Set(actions.map((a) => a.request_id).filter(Boolean)).size;
  const failed = actions.filter((a) => a.result?.status === "failed");
  const cancelled = actions.filter((a) => a.result?.status === "cancelled");
  return {
    observe_calls: observes.length,
    inspect_calls: 0, // runtime records no inspect event yet
    action_batches: batches,
    total_actions: actions.length,
    stale_frame_count: stale.length,
    cancelled_request_count: cancelled.length,
    timeout_count: failed.filter((a) => /timeout/i.test(a.result?.error || "")).length,
    recovery_count: 0, // runtime has no recovery mechanism/event (documented in README)
    user_takeover_count: takeovers.length,
    screenshot_bytes: observes.reduce((n, e) => n + (e.result?.screenshot_bytes || 0), 0),
    cancel_event_count: cancels.length,
    failed_action_count: failed.length,
    last_failed_action_error: failed.at(-1)?.result?.error ?? null,
    has_stale_rejection: stale.length > 0,
  };
}

/** Failure taxonomy assignment — heuristic, derived strictly from trace
 * events (never invented). Rules are documented in benchmarks/README.md;
 * they are calibrated against real runs and revised with data. */
export function classifyFailure({ success, entries, runTimedOut }) {
  if (success) {
    // Integrity gate: a task counts as a runtime success only when the model
    // actually drove the runtime — the trace must carry ≥1 observe and ≥1
    // action. A model that completed the goal through shell/scripting
    // (e.g. AppleScript via bash) without the MCP never exercised the
    // runtime; counting it would inflate the success rate dishonestly.
    const metrics = summarizeTrace(entries || []);
    if (metrics.total_actions === 0 || metrics.observe_calls === 0) {
      return "RUNTIME_NOT_DRIVEN";
    }
    return "SUCCESS";
  }
  const metrics = summarizeTrace(entries || []);
  const lastError = metrics.last_failed_action_error;

  if (runTimedOut) return "MODEL_PLANNING_ERROR"; // exceeded max_duration_seconds

  // Runtime-side signals first: trace records the evidence directly.
  if (entries.some((e) => e.event === "act.stale_rejected")) {
    // The model referenced a stale frame and the batch was refused; the
    // task still failed — the model failed to recover from the rejection.
    return "STALE_FRAME_RECOVERY_FAILED";
  }
  if (metrics.user_takeover_count > 0) return "CANCEL_FAILED"; // user grabbed the mouse mid-task
  if (lastError && /permission/i.test(lastError)) return "PERMISSION_ERROR";
  // The runtime's timeout wording is "request timed out: …" — match both
  // spellings so real failures land in ACTION_TIMEOUT.
  if (lastError && /timeout|timed out/i.test(lastError)) return "ACTION_TIMEOUT";

  if (metrics.total_actions === 0) {
    // The model produced no act call at all — it never drove the desktop.
    return "MODEL_STOPPED_EARLY";
  }
  if (lastError) {
    const last = entries.filter((e) => e.event === "action" && e.result?.status === "failed").at(-1);
    const type = last?.action?.type;
    if (type === "scroll") return "SCROLL_DIRECTION_ERROR";
    if (type === "drag") return "DRAG_FAILED";
    if (type === "type" || type === "type_text") {
      return /unicode|ime|clipboard/i.test(lastError) ? "UNICODE_INPUT_FAILED" : "TEXT_INPUT_FAILED";
    }
    if (type === "click") {
      return /small|target|miss/i.test(lastError) ? "SMALL_TARGET_MISS" : "GROUNDING_MISS";
    }
    return "MODEL_PLANNING_ERROR";
  }
  // Actions ran (some succeeded) but the outcome was never reached.
  return "MODEL_PLANNING_ERROR";
}

/** A short, honest root-cause excerpt for the report: the last failed
 * action's error and the last three trace events. */
export function failureDetail(entries, runTimedOut) {
  if (runTimedOut) return "model run exceeded max_duration_seconds";
  const failed = entries.filter((e) => e.event === "action" && e.result?.status === "failed").at(-1);
  const parts = [];
  if (failed) {
    parts.push(`last failed action: ${JSON.stringify(failed.action)} → ${failed.result?.error || "no detail"}`);
  }
  const tail = entries.slice(-3).map((e) => `${e.event}${e.error ? " (error)" : ""}`);
  parts.push(`last events: ${tail.join(", ") || "none"}`);
  return parts.join("; ");
}
