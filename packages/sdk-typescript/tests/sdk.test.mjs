// Hermetic tests for @computer-use/sdk: a fake JSON-RPC daemon on a temp
// Unix socket exercises the client without a real macOS daemon.
import { createServer } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import assert from "node:assert/strict";

import {
  connect,
  ComputerUseError,
  ComputerUseClient,
  TransportError,
  AbortError,
  ERROR_CODES,
  errorCodeName,
} from "../dist/index.js";

// --- Fake daemon -----------------------------------------------------------

function startFakeDaemon({ outOfOrder = false } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "cu-sdk-test-"));
  const socketPath = join(dir, "fake.sock");
  const conns = new Set();
  const server = createServer((conn) => {
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
        const respond = (obj) => conn.write(`${JSON.stringify(obj)}\n`);
        if (outOfOrder) {
          // Reply after a delay so responses land out of order relative to ids.
          setTimeout(() => respond(fakeResult(req, req.id)), 20);
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
          respond({ jsonrpc: "2.0", id: req.id, error: { code: -32009, message: "SESSION_NOT_FOUND", data: { code: "SESSION_NOT_FOUND", message: "no active session" } } });
        } else if (req.method === "takeover_active") {
          respond({ jsonrpc: "2.0", id: req.id, error: { code: -32016, message: "USER_TAKEOVER_ACTIVE", data: { code: "USER_TAKEOVER_ACTIVE", message: "The user has taken control. Call release before resuming agent control." } } });
        } else if (req.method === "timeout") {
          respond({ jsonrpc: "2.0", id: req.id, error: { code: -32017, message: "ACTION_TIMEOUT", data: { code: "ACTION_TIMEOUT", message: "request timed out", method: "computer.act" } } });
        } else if (req.method === "capture") {
          respond({ jsonrpc: "2.0", id: req.id, error: { code: -32018, message: "CAPTURE_FAILED", data: { code: "CAPTURE_FAILED", message: "screen capture failed" } } });
        } else if (req.method === "hang") {
          // Never respond.
        } else if (req.method === "computer.act") {
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
        } else if (req.method === "computer.session") {
          const action = req.params?.action;
          if (action === "start") {
            respond({ jsonrpc: "2.0", id: req.id, result: { session_id: req.params?.session_id ?? "s9", state: "active", paused: false, user_takeover: false, lock_held: true, display_id: "1", created_at: "2026-08-03T00:00:00Z", started_by: "test" } });
          } else {
            respond({ jsonrpc: "2.0", id: req.id, result: { session_id: "s1", state: "active", paused: false, user_takeover: false, lock_held: true, display_id: "1", created_at: "2026-08-03T00:00:00Z", started_by: "test" } });
          }
        } else if (req.method === "runtime.health") {
          respond({ jsonrpc: "2.0", id: req.id, result: { version: "0.1.0", ready: true, permissions: { screen_recording: true, accessibility: true }, active_sessions: 1, uptime_secs: 5, frame_cache: 2 } });
        } else {
          respond({ jsonrpc: "2.0", id: req.id, result: null });
        }
      }
    });
  });
  const fakeResult = (req, id) => ({ jsonrpc: "2.0", id, result: { echo: req.method, id } });
  server.listen(socketPath);
  return { socketPath, server, dir, conns };
}

const stopFakeDaemon = (fake) => {
  for (const conn of fake.conns) conn.destroy();
  fake.server.close();
  rmSync(fake.dir, { recursive: true, force: true });
};

// --- Tests -------------------------------------------------------------------

test("round-trip request and structured result", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const result = await client.request("echo", { hello: "world" });
    assert.deepEqual(result, { echoed: { hello: "world" } });
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("daemon errors map to ComputerUseError with machine-readable code", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const err = await client.request("fault").catch((e) => e);
    assert.ok(err instanceof ComputerUseError);
    assert.equal(err.code, "STALE_FRAME");
    assert.equal(err.jsonrpcCode, -32003);
    assert.equal(err.data.referenced_frame_id, "f1");
    assert.equal(err.data.change_score, 42);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("paused error exposes PAUSED code", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const err = await client.request("paused").catch((e) => e);
    assert.ok(err instanceof ComputerUseError);
    assert.equal(err.code, "PAUSED");
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("out-of-order responses are matched by id", async () => {
  const fake = startFakeDaemon({ outOfOrder: true });
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const [a, b, c] = await Promise.all([
      client.request("a"),
      client.request("b"),
      client.request("c"),
    ]);
    assert.equal(a.id, 1);
    assert.equal(b.id, 2);
    assert.equal(c.id, 3);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("aborting with a signal rejects with AbortError and drops the request", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const ac = new AbortController();
    // Fire after the request is in flight (daemon hangs on "hang").
    setTimeout(() => ac.abort(), 20);
    const err = await client.request("hang", undefined, { signal: ac.signal }).catch((e) => e);
    assert.ok(err instanceof AbortError);
    assert.match(err.message, /aborted/);
    // The client stays usable after an abort.
    const echoed = await client.request("echo", { ok: true });
    assert.deepEqual(echoed.echoed, { ok: true });
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("a pre-aborted signal rejects immediately", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const ac = new AbortController();
    ac.abort();
    const err = await client.request("hang", undefined, { signal: ac.signal }).catch((e) => e);
    assert.ok(err instanceof AbortError);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("request timeout rejects with TransportError", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const err = await client.request("hang", undefined, 100).catch((e) => e);
    assert.ok(err instanceof TransportError);
    assert.match(err.message, /timed out/);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("connect to a missing socket rejects with TransportError", async () => {
  const dir = mkdtempSync(join(tmpdir(), "cu-sdk-test-"));
  try {
    const err = await connect({ socketPath: join(dir, "nope.sock") }).catch((e) => e);
    assert.ok(err instanceof TransportError);
    assert.match(err.message, /is the daemon running/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("ensureSession resolves the active session", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const s = await client.ensureSession();
    assert.equal(s.session_id, "s1");
    assert.equal(s.state, "active");
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("ensureSession starts one when none exists", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    // First call (status) reports no active session; the retry (start)
    // delegates to the real transport.
    const realRequest = client.request.bind(client);
    let calls = 0;
    client.request = (method, params) => {
      calls += 1;
      if (calls === 1) {
        return Promise.reject(
          new ComputerUseError(-32009, "SESSION_NOT_FOUND", { code: "SESSION_NOT_FOUND", message: "no active session" }),
        );
      }
      return realRequest(method, params);
    };
    const started = await client.ensureSession();
    assert.equal(started.session_id, "s9");
    assert.equal(calls, 2);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("unconnected client rejects request()", async () => {
  const client = new ComputerUseClient({ socketPath: "/nonexistent" });
  const err = await client.request("ping").catch((e) => e);
  assert.ok(err instanceof TransportError);
});

test("errorCodeName maps jsonrpc codes", () => {
  assert.equal(errorCodeName(-32003), "STALE_FRAME");
  assert.equal(errorCodeName(-32005), "PAUSED");
  assert.equal(errorCodeName(-32016), "USER_TAKEOVER_ACTIVE");
  assert.equal(errorCodeName(-32017), "ACTION_TIMEOUT");
  assert.equal(errorCodeName(-32018), "CAPTURE_FAILED");
  assert.equal(errorCodeName(-32700), "PARSE_ERROR");
  assert.equal(errorCodeName(-9999), ERROR_CODES.INTERNAL);
});

test("the full spec error taxonomy is exported (13 canonical names + aliases)", () => {
  // Canonical programmatic names for every failure class in the spec.
  assert.equal(ERROR_CODES.DAEMON_UNAVAILABLE, "DAEMON_UNAVAILABLE");
  assert.equal(ERROR_CODES.PERMISSION_DENIED, "PERMISSION");
  assert.equal(ERROR_CODES.SESSION_NOT_FOUND, "SESSION_NOT_FOUND");
  assert.equal(ERROR_CODES.SESSION_PAUSED, "PAUSED");
  assert.equal(ERROR_CODES.USER_TAKEOVER_ACTIVE, "USER_TAKEOVER_ACTIVE");
  assert.equal(ERROR_CODES.CONTROL_LOCKED, "CONTROL_LOCKED");
  assert.equal(ERROR_CODES.STALE_FRAME, "STALE_FRAME");
  assert.equal(ERROR_CODES.OUT_OF_BOUNDS, "OUT_OF_BOUNDS");
  assert.equal(ERROR_CODES.ACTION_CANCELLED, "CANCELLED");
  assert.equal(ERROR_CODES.ACTION_TIMEOUT, "ACTION_TIMEOUT");
  assert.equal(ERROR_CODES.CAPTURE_FAILED, "CAPTURE_FAILED");
  assert.equal(ERROR_CODES.INVALID_REQUEST, "INVALID_REQUEST");
  assert.equal(ERROR_CODES.TRACE_UNAVAILABLE, "TRACE_ERROR");
  // The wire codes resolve to their canonical names.
  assert.equal(errorCodeName(-32016), ERROR_CODES.USER_TAKEOVER_ACTIVE);
  assert.equal(errorCodeName(-32017), ERROR_CODES.ACTION_TIMEOUT);
  assert.equal(errorCodeName(-32018), ERROR_CODES.CAPTURE_FAILED);
});

test("new daemon error codes surface on ComputerUseError", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const t = await client.request("takeover_active").catch((e) => e);
    assert.equal(t.code, "USER_TAKEOVER_ACTIVE");
    assert.equal(t.jsonrpcCode, -32016);
    const x = await client.request("timeout").catch((e) => e);
    assert.equal(x.code, "ACTION_TIMEOUT");
    assert.equal(x.jsonrpcCode, -32017);
    const c = await client.request("capture").catch((e) => e);
    assert.equal(c.code, "CAPTURE_FAILED");
    assert.equal(c.jsonrpcCode, -32018);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("TransportError carries the DAEMON_UNAVAILABLE code", async () => {
  const dir = mkdtempSync(join(tmpdir(), "cu-sdk-unavail-"));
  try {
    const missing = join(dir, "missing.sock");
    const err = await connect({ socketPath: missing }).catch((e) => e);
    assert.ok(err instanceof TransportError);
    assert.equal(err.code, "DAEMON_UNAVAILABLE");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("act result exposes stabilization and trace reports", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const res = await client.act({
      session_id: "s1",
      frame_id: "frame_1",
      actions: [{ type: "wait", duration_ms: 1 }],
    });
    assert.equal(res.stabilization.outcome, "timed_out");
    assert.equal(res.stabilization.change_score, 0.31);
    assert.equal(res.stabilization.elapsed_ms, 2000);
    assert.equal(res.trace.mode, "best_effort");
    assert.equal(res.trace.degraded, false);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("convenience wrappers pass params through", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const health = await client.health();
    assert.equal(health.ready, true);
    assert.equal(health.permissions.screen_recording, true);
    const status = await client.session("status");
    assert.equal(status.state, "active");
    const started = await client.session("start", { session_id: "s9" });
    assert.equal(started.session_id, "s9");
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});
