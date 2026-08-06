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
import {
  START_TOKEN,
  START_OBSERVATION_TOKEN,
  ADMIN_TOKEN,
  startFakeDaemon,
  stopFakeDaemon,
} from "./fake-daemon.mjs";

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
    // The cancel chain is authorized by the session's control token — a real
    // client holds one for the sessions it started, so start one first.
    const started = await client.session("start", { display_id: "1" });
    const ac = new AbortController();
    setTimeout(() => ac.abort(), 20);
    const err = await client
      .request("hang", { session_id: started.session_id }, { signal: ac.signal })
      .catch((e) => e);
    assert.ok(err instanceof AbortError);
    // The full cancel chain: the daemon must receive a fire-and-forget
    // computer.cancel for the session so the server-side batch stops too.
    await new Promise((r) => setTimeout(r, 50));
    const cancelReq = fake.requests.find((r) => r.method === "computer.cancel");
    assert.ok(cancelReq, "expected a computer.cancel notify after abort");
    assert.equal(cancelReq.params.session_id, started.session_id);
    assert.equal(cancelReq.params.control_token, START_TOKEN, "the notify carries the control token");
    // Precision: the cancel pins the aborted request's own id (3 — id 1 was
    // connect()'s protocol-version check, id 2 the session start) on this
    // connection, so an abort can never cancel a different client's request
    // with the same id.
    assert.equal(cancelReq.params.request_id, 3);
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

test("sensitive reads carry the observation token from the credential", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const started = await client.session("start");
    // observe / inspect / trace reads are sensitive — the SDK injects the
    // observation token it holds (start issued it) without the caller asking.
    await client.observe({ session_id: started.session_id });
    await client.inspect({
      session_id: started.session_id,
      frame_id: "f1",
      region: { x: 0, y: 0, width: 1, height: 1, coordinate_space: "image_pixels" },
    });
    await client.traceGet(started.session_id);
    // Round 7: trace.export is a pure read — no destination path in the
    // request, content + sha256 in the result.
    const exportResult = await client.traceExport(started.session_id);
    assert.equal(exportResult.format, "jsonl");
    assert.equal(exportResult.session_id, started.session_id);
    assert.ok(exportResult.content.includes('"event"'), "inline content");
    assert.equal(exportResult.sha256, "abc123def456", "fake sha256 echoed");
    assert.ok(!("path" in exportResult), "no filesystem path in the result");
    await client.traceReplay(started.session_id);
    for (const method of [
      "computer.observe",
      "computer.inspect",
      "trace.get",
      "trace.export",
      "trace.replay",
    ]) {
      const req = fake.requests.find((r) => r.method === method);
      assert.ok(req, `${method} was sent`);
      assert.equal(
        req.params.observation_token,
        START_OBSERVATION_TOKEN,
        `${method} carries the session's observation token`,
      );
    }
    // A status without a session_id resolves the active session — still a
    // sensitive read, so the held credential's token rides along.
    await client.session("status");
    const statusReq = fake.requests.find((r) => r.method === "computer.session" && r.params.action === "status");
    assert.ok(statusReq, "status was sent");
    assert.equal(statusReq.params.observation_token, START_OBSERVATION_TOKEN);

    // An explicit token always wins over the injected one.
    await client.observe({ session_id: started.session_id, observation_token: "explicit-obs" });
    const explicit = fake.requests.filter((r) => r.method === "computer.observe").at(-1);
    assert.equal(explicit.params.observation_token, "explicit-obs");
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("trace list/summaries are session-scoped since round 6", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const started = await client.session("start");

    // traceList(sessionId) injects the session's observation token and
    // addresses exactly that session.
    const list = await client.traceList(started.session_id);
    assert.equal(list.traces.length, 1);
    assert.equal(list.traces[0].session_id, started.session_id);
    const listReq = fake.requests.find((r) => r.method === "trace.list");
    assert.ok(listReq, "trace.list was sent");
    assert.equal(listReq.params.session_id, started.session_id, "session-scoped params");
    assert.equal(listReq.params.observation_token, START_OBSERVATION_TOKEN);

    // traceSummaries(sessionId) behaves the same, returning the bare array.
    const summaries = await client.traceSummaries(started.session_id);
    assert.equal(summaries.length, 1);
    const sumsReq = fake.requests.find((r) => r.method === "trace.summaries");
    assert.ok(sumsReq, "trace.summaries was sent");
    assert.equal(sumsReq.params.session_id, started.session_id);
    assert.equal(sumsReq.params.observation_token, START_OBSERVATION_TOKEN);

    // An explicit token still wins (supplied by the caller, verified against
    // the session like any presented token); a limit rides along untouched.
    await client.traceSummaries(started.session_id, {
      observationToken: START_OBSERVATION_TOKEN,
      limit: 5,
    });
    const explicit = fake.requests.filter((r) => r.method === "trace.summaries").at(-1);
    assert.equal(explicit.params.observation_token, START_OBSERVATION_TOKEN);
    assert.equal(explicit.params.limit, 5);

    // A token for another session must never be injected: a client holding a
    // credential for session A calling traceList(B) sends no token at all,
    // and the daemon refuses the request (OBSERVATION_TOKEN_REQUIRED) — a
    // cross-session capability never leaks.
    await client.session("stop");
    const again = await client.session("start");
    assert.notEqual(again.session_id, started.session_id);
    await assert.rejects(
      () => client.traceList(started.session_id),
      (err) => err.data?.code !== undefined,
      "a session-A token must not read session-B traces",
    );
    const cross = fake.requests.filter((r) => r.method === "trace.list").at(-1);
    assert.equal(cross.params.session_id, started.session_id);
    assert.equal(cross.params.observation_token, undefined, "no token injected cross-session");
    assert.equal(cross.params.control_token, undefined);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("session.summary is the public tokenless view", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const empty = await client.sessionSummary();
    assert.equal(empty.session_id, null);
    assert.equal(empty.state, null);
    assert.equal(empty.lock_held, false);
    await client.session("start");
    const full = await client.sessionSummary();
    assert.equal(full.session_id, "s1");
    assert.equal(full.state, "active");
    assert.equal(full.lock_held, true);
    // The coarse view never carried a capability token — and its result type
    // has no slot for one (the fake models the daemon's shape).
    const req = fake.requests.find((r) => r.method === "session.summary");
    assert.ok(req, "summary request was sent");
    assert.equal(req.params.observation_token, undefined);
    assert.equal(req.params.control_token, undefined);
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

test("shutdown requires the admin token, never a session token", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    // Missing token → DAEMON_ADMIN_TOKEN_REQUIRED.
    const missing = await client.shutdown("").catch((e) => e);
    assert.ok(missing instanceof ComputerUseError);
    assert.equal(missing.code, "DAEMON_ADMIN_TOKEN_REQUIRED");

    // A session control token must never shut the daemon down.
    await client.session("start");
    const wrong = await client.shutdown(START_TOKEN).catch((e) => e);
    assert.ok(wrong instanceof ComputerUseError);
    assert.equal(wrong.code, "INVALID_DAEMON_ADMIN_TOKEN");
    assert.equal(fake.state.shutDown, undefined, "daemon must stay up");

    // The correct admin token shuts it down, carrying the token in params.
    const result = await client.shutdown(ADMIN_TOKEN);
    assert.deepEqual(result, { status: "shutting_down" });
    assert.equal(fake.state.shutDown, true);
    const shutdownReqs = fake.requests.filter((r) => r.method === "runtime.shutdown");
    const req = shutdownReqs[shutdownReqs.length - 1];
    assert.equal(req.params.admin_token, ADMIN_TOKEN);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});
