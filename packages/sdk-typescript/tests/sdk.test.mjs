// Hermetic tests for @computer-use/sdk: a fake JSON-RPC daemon on a temp
// Unix socket exercises the client without a real macOS daemon.
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
import { startFakeDaemon, stopFakeDaemon } from "./fake-daemon.mjs";

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
    // ids are strictly increasing (connect() itself used an earlier id for the
    // protocol-version check); each response still carries its own request id.
    assert.ok(a.id < b.id && b.id < c.id, `ids increasing: ${a.id} < ${b.id} < ${c.id}`);
    assert.equal(a.echo, "a");
    assert.equal(b.echo, "b");
    assert.equal(c.echo, "c");
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

test("aborting an in-flight request notifies the daemon with a precise computer.cancel", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const ac = new AbortController();
    setTimeout(() => ac.abort(), 20);
    const err = await client.request("hang", { session_id: "s1" }, { signal: ac.signal }).catch((e) => e);
    assert.ok(err instanceof AbortError);
    // The full cancel chain: the daemon must receive a fire-and-forget
    // computer.cancel for the session so the server-side batch stops too.
    await new Promise((r) => setTimeout(r, 50));
    const cancelReq = fake.requests.find((r) => r.method === "computer.cancel");
    assert.ok(cancelReq, "expected a computer.cancel notify after abort");
    assert.equal(cancelReq.params.session_id, "s1");
    // Precision: the cancel pins the aborted request's own id (2 — id 1 was
    // connect()'s protocol-version check) on this connection, so an abort can
    // never cancel a different client's request with the same id.
    assert.equal(cancelReq.params.request_id, 2);
    // The client stays usable after the abort.
    const echoed = await client.request("echo", { ok: true });
    assert.deepEqual(echoed.echoed, { ok: true });
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

test("ensureSession resolves the active session without starting another", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const created = await client.session("start");
    assert.equal(fake.state.startCount, 1);
    const s = await client.ensureSession();
    assert.equal(s.session_id, created.session_id);
    // The existing session is reused; no second start happened.
    assert.equal(fake.state.startCount, 1);
    assert.equal(s.state, "active");
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("ensureSession auto-starts when the daemon has no active session", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    // Daemon starts with no session (stateful fake).
    const s = await client.ensureSession();
    assert.equal(s.session_id, "s1");
    assert.equal(s.state, "active");
    // Exactly one start request happened, carrying this client's identity.
    assert.equal(fake.state.startCount, 1);
    assert.equal(fake.state.startCalls[0].client_id, "sdk");
    assert.equal(fake.state.startCalls[0].client_name, "TypeScript SDK");
    assert.ok(fake.state.startCalls[0].client_instance_id);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("concurrent ensureSession calls start exactly one session", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const [a, b, c] = await Promise.all([
      client.ensureSession(),
      client.ensureSession(),
      client.ensureSession(),
    ]);
    assert.equal(fake.state.startCount, 1, "single-flight: one start for three callers");
    assert.equal(a.session_id, "s1");
    assert.equal(b.session_id, a.session_id);
    assert.equal(c.session_id, a.session_id);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("ensureSession rethrows non-SESSION_NOT_FOUND errors without starting", async () => {
  const fake = startFakeDaemon({
    statusError: {
      code: -32002,
      message: "PERMISSION",
      data: { code: "PERMISSION", message: "screen recording permission missing" },
    },
  });
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const err = await client.ensureSession().catch((e) => e);
    assert.ok(err instanceof ComputerUseError);
    assert.equal(err.code, "PERMISSION");
    assert.equal(fake.state.startCount, 0, "a permission failure must not trigger start");
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
  assert.equal(errorCodeName(-32019), "CONTROL_TOKEN_REQUIRED");
  assert.equal(errorCodeName(-32020), "INVALID_CONTROL_TOKEN");
  assert.equal(errorCodeName(-32021), "SESSION_STOPPED");
  assert.equal(errorCodeName(-32022), "REQUEST_TIMEOUT");
  assert.equal(errorCodeName(-32023), "PROTOCOL_VERSION_MISMATCH");
  assert.equal(errorCodeName(-32700), "PARSE_ERROR");
  assert.equal(errorCodeName(-9999), ERROR_CODES.INTERNAL);
});

test("the full spec error taxonomy is exported (18 canonical names + aliases)", () => {
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
  assert.equal(ERROR_CODES.CONTROL_TOKEN_REQUIRED, "CONTROL_TOKEN_REQUIRED");
  assert.equal(ERROR_CODES.INVALID_CONTROL_TOKEN, "INVALID_CONTROL_TOKEN");
  assert.equal(ERROR_CODES.SESSION_STOPPED, "SESSION_STOPPED");
  assert.equal(ERROR_CODES.REQUEST_TIMEOUT, "REQUEST_TIMEOUT");
  assert.equal(ERROR_CODES.PROTOCOL_VERSION_MISMATCH, "PROTOCOL_VERSION_MISMATCH");
  assert.equal(ERROR_CODES.INVALID_REQUEST, "INVALID_REQUEST");
  assert.equal(ERROR_CODES.TRACE_UNAVAILABLE, "TRACE_ERROR");
  // The wire codes resolve to their canonical names.
  assert.equal(errorCodeName(-32016), ERROR_CODES.USER_TAKEOVER_ACTIVE);
  assert.equal(errorCodeName(-32017), ERROR_CODES.ACTION_TIMEOUT);
  assert.equal(errorCodeName(-32018), ERROR_CODES.CAPTURE_FAILED);
  assert.equal(errorCodeName(-32023), ERROR_CODES.PROTOCOL_VERSION_MISMATCH);
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
    assert.equal(health.active_sessions, 0, "stateful daemon starts without a session");
    const started = await client.session("start");
    assert.equal(started.session_id, "s1");
    assert.equal(started.owner_client_id, "sdk");
    const status = await client.session("status");
    assert.equal(status.state, "active");
    assert.equal(status.session_id, started.session_id);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("session lifecycle actions mutate state on the stateful fake", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const started = await client.session("start");
    assert.equal(started.state, "active");
    const paused = await client.session("pause", { session_id: started.session_id });
    assert.equal(paused.paused, true);
    const resumed = await client.session("resume", { session_id: started.session_id });
    assert.equal(resumed.paused, false);
    const takeover = await client.session("takeover", { session_id: started.session_id });
    assert.equal(takeover.user_takeover, true);
    const released = await client.session("release", { session_id: started.session_id });
    assert.equal(released.user_takeover, false);
    await client.session("stop", { session_id: started.session_id });
    const notFound = await client.session("status").catch((e) => e);
    assert.ok(notFound instanceof ComputerUseError);
    assert.equal(notFound.code, "SESSION_NOT_FOUND");
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});
