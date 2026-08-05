// Shared fake JSON-RPC daemon for the SDK tests: a temp Unix socket server
// that maintains real session state — the same shape as the real runtime.
//
// - No session initially; `start` creates one (recording the caller's client
//   identity) and issues the control token exactly once, like the real daemon
//   (which stores only a hash and never repeats it). `status` never carries it.
// - `stop` removes the session; pause/resume/takeover/release mutate the state.
// - `runtime.version` answers truthfully: the client verifies protocol_version
//   on connect, so a fake that ignored it would make every test fail.
// - Every received request is recorded as { conn, ...req } so tests can assert
//   which connection sent what (per-connection request-id isolation).
import { createServer } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

export function mkSession(id, over = {}) {
  return {
    session_id: id,
    state: "active",
    paused: false,
    user_takeover: false,
    lock_held: true,
    display_id: "1",
    created_at: "2026-08-03T00:00:00Z",
    started_by: "fake",
    ...over,
  };
}

// The daemon issues both capability tokens exactly once, in the `start`
// response; `status` and every other result never carry either. The fake
// models that — and (like the real daemon) refuses tokenless sensitive reads
// with OBSERVATION_TOKEN_REQUIRED.
export const START_TOKEN = "fake-control-token-for-tests";
export const START_OBSERVATION_TOKEN = "fake-observation-token-for-tests";

// The daemon's admin token — held only by the daemon manager (CLI /
// LaunchAgent); a session capability token never authorizes shutdown.
export const ADMIN_TOKEN = "fake-daemon-admin-token-for-tests";

export const ADMIN_REQUIRED_ERROR = {
  code: -32026,
  message: "DAEMON_ADMIN_TOKEN_REQUIRED",
  data: {
    code: "DAEMON_ADMIN_TOKEN_REQUIRED",
    message: "runtime.shutdown requires the daemon admin token.",
  },
};

export const INVALID_ADMIN_ERROR = {
  code: -32027,
  message: "INVALID_DAEMON_ADMIN_TOKEN",
  data: {
    code: "INVALID_DAEMON_ADMIN_TOKEN",
    message: "The presented admin token does not match this daemon.",
  },
};

export const OBSERVATION_REQUIRED_ERROR = {
  code: -32024,
  message: "OBSERVATION_TOKEN_REQUIRED",
  data: {
    code: "OBSERVATION_TOKEN_REQUIRED",
    message:
      "This operation requires the session observation token (or its control token). A session id alone grants no observation permission.",
  },
};

export const INVALID_OBSERVATION_ERROR = {
  code: -32025,
  message: "INVALID_OBSERVATION_TOKEN",
  data: {
    code: "INVALID_OBSERVATION_TOKEN",
    message:
      "The presented observation token (or control token) does not match this session.",
  },
};

export const NOT_FOUND_ERROR = {
  code: -32009,
  message: "SESSION_NOT_FOUND",
  data: { code: "SESSION_NOT_FOUND", message: "No active computer-use session exists." },
};

export function startFakeDaemon({
  outOfOrder = false,
  statusError = null,
  actDelayMs = 0,
  cancelAck = true,
  // Pre-seed an active session owned by another client (for policy tests).
  seedSession = null,
  // With a session present, `start` answers CONTROL_LOCKED (mirroring the
  // real daemon's control lock) instead of overwriting the session.
  controlLockOnStart = false,
} = {}) {
  const dir = mkdtempSync(join(tmpdir(), "cu-sdk-test-"));
  const socketPath = join(dir, "fake.sock");
  const conns = new Set();
  let connCounter = 0;
  // Stateful session store: { session, startCount, startCalls }; every
  // received request is recorded so cancel notifications are observable.
  const state = { session: seedSession, startCount: 0, startCalls: [] };
  const requests = [];
  const server = createServer((conn) => {
    const connId = ++connCounter;
    conns.add(conn);
    conn.on("close", () => conns.delete(conn));
    let buffer = "";
    conn.on("data", (chunk) => {
      buffer += chunk.toString("utf8");
      let nl;
      while ((nl = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, nl).trim();
        buffer = buffer.slice(nl + 1);
        if (!line) continue;
        const req = JSON.parse(line);
        requests.push({ conn: connId, ...req });
        const respond = (obj) => conn.write(`${JSON.stringify(obj)}\n`);
        if (outOfOrder) {
          // Reply after a delay so responses land out of order relative to
          // ids. runtime.version must still answer truthfully — connect()
          // verifies the protocol version before any request is issued.
          setTimeout(() => {
            if (req.method === "runtime.version") {
              respond({ jsonrpc: "2.0", id: req.id, result: { name: "fake", version: "0.1.0", protocol_version: 3 } });
            } else {
              respond({ jsonrpc: "2.0", id: req.id, result: { echo: req.method, id: req.id } });
            }
          }, 20);
        } else if (req.method === "echo") {
          respond({ jsonrpc: "2.0", id: req.id, result: { echoed: req.params } });
        } else if (req.method === "fault") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            error: { code: -32003, message: "STALE_FRAME", data: { code: "STALE_FRAME", message: "frame stale", referenced_frame_id: "f1", current_frame_id: "f2", change_score: 42, reason: "app_changed" } },
          });
        } else if (req.method === "paused") {
          respond({ jsonrpc: "2.0", id: req.id, error: { code: -32005, message: "PAUSED", data: { code: "PAUSED", message: "session paused" } } });
        } else if (req.method === "not_found") {
          respond({ jsonrpc: "2.0", id: req.id, error: NOT_FOUND_ERROR });
        } else if (req.method === "takeover_active") {
          respond({ jsonrpc: "2.0", id: req.id, error: { code: -32016, message: "USER_TAKEOVER_ACTIVE", data: { code: "USER_TAKEOVER_ACTIVE", message: "The user has taken control. Call release before resuming agent control." } } });
        } else if (req.method === "timeout") {
          respond({ jsonrpc: "2.0", id: req.id, error: { code: -32017, message: "ACTION_TIMEOUT", data: { code: "ACTION_TIMEOUT", message: "request timed out", method: "computer.act" } } });
        } else if (req.method === "capture") {
          respond({ jsonrpc: "2.0", id: req.id, error: { code: -32018, message: "CAPTURE_FAILED", data: { code: "CAPTURE_FAILED", message: "screen capture failed" } } });
        } else if (req.method === "hang") {
          // Never respond.
        } else if (req.method === "computer.act") {
          // `actDelayMs > 0` delays the response — the window in which a
          // client-side timeout fires and sends computer.cancel.
          const reply = () =>
            respond({
              jsonrpc: "2.0",
              id: req.id,
              result: {
                executed: true,
                action_results: [{ index: 0, status: "success", duration_ms: 5 }],
                screen_changed: false,
                stable: true,
                next_frame_id: "frame_9",
                stabilization: { outcome: "timed_out", change_score: 0.31, samples: 7, elapsed_ms: 2000 },
                trace: { mode: "best_effort", degraded: false, warnings: [] },
              },
            });
          if (actDelayMs > 0) setTimeout(reply, actDelayMs);
          else reply();
        } else if (req.method === "computer.cancel") {
          // With `cancelAck: false` the fake never acknowledges — the SDK must
          // then report the timeout with runtimeCancellationConfirmed: false.
          if (cancelAck) {
            respond({
              jsonrpc: "2.0",
              id: req.id,
              result: { cancelled: true, session_id: req.params?.session_id },
            });
          }
        } else if (req.method === "computer.session") {
          const action = req.params?.action;
          if (action === "start") {
            state.startCount += 1;
            state.startCalls.push(req.params);
            if (controlLockOnStart && state.session) {
              // Mirrors the real daemon's CONTROL_LOCKED wire shape: message
              // is the code; `data` carries the holder's non-secret identity.
              respond({
                jsonrpc: "2.0",
                id: req.id,
                error: {
                  code: -32004,
                  message: "CONTROL_LOCKED",
                  data: {
                    holder: state.session.session_id,
                    owner: {
                      client_id: state.session.owner_client_id,
                      client_name: state.session.owner_client_name,
                      client_instance_id: state.session.owner_instance_id,
                    },
                  },
                },
              });
              return;
            }
            const id = `s${state.startCount}`;
            state.session = mkSession(id, {
              started_by: req.params?.client_name ?? "fake",
              owner_client_id: req.params?.client_id ?? "fake",
              owner_client_name: req.params?.client_name,
              owner_instance_id: req.params?.client_instance_id,
              // Issued exactly once, in the start response, like the real
              // daemon (which stores only a hash and never repeats either).
              control_token: START_TOKEN,
              observation_token: START_OBSERVATION_TOKEN,
            });
            respond({ jsonrpc: "2.0", id: req.id, result: state.session });
          } else if (action === "status") {
            if (statusError) {
              respond({ jsonrpc: "2.0", id: req.id, error: statusError });
            } else if (!state.session) {
              respond({ jsonrpc: "2.0", id: req.id, error: NOT_FOUND_ERROR });
            } else if (!presentedToken(req.params)) {
              // Status is a sensitive read in v3: a session id alone grants
              // no observation permission.
              respond({ jsonrpc: "2.0", id: req.id, error: OBSERVATION_REQUIRED_ERROR });
            } else if (!hasToken(req.params)) {
              // A token was presented but none verified — non-descriptive.
              respond({ jsonrpc: "2.0", id: req.id, error: INVALID_OBSERVATION_ERROR });
            } else {
              const { control_token, observation_token, ...safe } = state.session;
              void control_token;
              void observation_token;
              respond({ jsonrpc: "2.0", id: req.id, result: safe });
            }
          } else if (action === "stop") {
            if (!state.session) {
              respond({ jsonrpc: "2.0", id: req.id, error: NOT_FOUND_ERROR });
            } else {
              const stopped = { ...state.session, state: "stopped", paused: false, user_takeover: false, lock_held: false };
              state.session = null;
              respond({ jsonrpc: "2.0", id: req.id, result: stopped });
            }
          } else {
            // pause / resume / takeover / release mutate the stored session.
            if (!state.session) {
              respond({ jsonrpc: "2.0", id: req.id, error: NOT_FOUND_ERROR });
            } else {
              const s = state.session;
              if (action === "pause") state.session = { ...s, state: "paused", paused: true };
              if (action === "resume") state.session = { ...s, state: "active", paused: false };
              if (action === "takeover") state.session = { ...s, state: "user_takeover", user_takeover: true, paused: false };
              if (action === "release") state.session = { ...s, state: "active", user_takeover: false };
              respond({ jsonrpc: "2.0", id: req.id, result: state.session });
            }
          }
        } else if (req.method === "runtime.version") {
          respond({ jsonrpc: "2.0", id: req.id, result: { name: "fake", version: "0.1.0", protocol_version: 3 } });
        } else if (req.method === "session.summary") {
          // The public coarse view: tokenless, never carries capability tokens.
          // When `statusError` is configured it is honored here too — a daemon
          // that cannot answer a read cannot answer *any* read, and the SDK
          // must surface that instead of silently starting a new session.
          if (statusError) {
            respond({ jsonrpc: "2.0", id: req.id, error: statusError });
            return;
          }
          const s = state.session;
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: {
              session_id: s?.session_id ?? null,
              state: s?.state ?? null,
              lock_held: s?.lock_held ?? false,
              owner_client_id: s?.owner_client_id ?? null,
              owner_client_name: s?.owner_client_name ?? null,
              message: s ? "knowing its id grants no observation or control permission" : null,
            },
          });
        } else if (req.method === "runtime.health") {
          respond({ jsonrpc: "2.0", id: req.id, result: { version: "0.1.0", ready: true, permissions: { screen_recording: true, accessibility: true }, active_sessions: state.session ? 1 : 0, uptime_secs: 5, frame_cache: 2 } });
        } else if (req.method === "runtime.shutdown") {
          // Only the admin token may shut the daemon down — not a session
          // capability token, not nothing.
          const presented = req.params?.admin_token;
          if (!presented) {
            respond({ jsonrpc: "2.0", id: req.id, error: ADMIN_REQUIRED_ERROR });
          } else if (presented !== ADMIN_TOKEN) {
            respond({ jsonrpc: "2.0", id: req.id, error: INVALID_ADMIN_ERROR });
          } else {
            state.shutDown = true;
            respond({ jsonrpc: "2.0", id: req.id, result: { status: "shutting_down" } });
          }
        } else if (req.method === "trace.list" || req.method === "trace.summaries") {
          // Session-scoped since round 6: the request must address exactly one
          // session with that session's observation/control token. The fake
          // mirrors the real daemon — a token from a different session is
          // refused; a missing token is refused with OBSERVATION_TOKEN_REQUIRED.
          const sid = req.params?.session_id;
          if (typeof sid !== "string" || !sid) {
            respond({
              jsonrpc: "2.0",
              id: req.id,
              error: {
                code: -32602,
                message: "INVALID_PARAMS",
                data: { code: "INVALID_PARAMS", message: "Missing required parameter: session_id" },
              },
            });
          } else if (state.session?.session_id !== sid) {
            respond({ jsonrpc: "2.0", id: req.id, error: NOT_FOUND_ERROR });
          } else if (!hasToken(req.params)) {
            respond({ jsonrpc: "2.0", id: req.id, error: OBSERVATION_REQUIRED_ERROR });
          } else {
            const summary = {
              trace_id: sid,
              session_id: sid,
              event_count: 2,
              size_bytes: 128,
              created_at: "2026-08-03T00:00:00Z",
            };
            const result =
              req.method === "trace.list" ? { traces: [summary] } : [summary];
            respond({ jsonrpc: "2.0", id: req.id, result });
          }
        } else {
          respond({ jsonrpc: "2.0", id: req.id, result: null });
        }
      }
    });
  });
  server.listen(socketPath);
  // A failed test must not hang the runner: with the server unref'd, Node
  // exits even if a test error skipped stopFakeDaemon().
  server.unref();
  return { socketPath, server, dir, conns, state, requests };
}

// A sensitive-read request (status / observe / inspect / trace) carries a
// token when the caller holds one: either slot, either token.
export const hasToken = (params) =>
  Boolean(
    params &&
      (params.observation_token === START_OBSERVATION_TOKEN ||
        params.control_token === START_TOKEN),
  );

// Whether *any* token was presented (valid or not) — the fake distinguishes
// "no credential at all" (-32024) from "credential did not verify" (-32025),
// exactly like the real daemon's verify_read_tokens.
export const presentedToken = (params) =>
  Boolean(params && (typeof params.observation_token === "string" || typeof params.control_token === "string"));

export const stopFakeDaemon = (fake) => {
  for (const conn of fake.conns) conn.destroy();
  fake.server.close();
  rmSync(fake.dir, { recursive: true, force: true });
};
