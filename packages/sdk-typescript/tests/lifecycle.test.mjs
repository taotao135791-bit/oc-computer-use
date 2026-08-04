// §十八 SDK request-lifecycle tests: listener hygiene, precise cancellation,
// timeout→cancel→ack, per-connection request-id isolation, and close cleanup.
//
// The failures these guard against are the classic SDK leaks: an abort
// listener that outlives its request, a timeout that abandons the server-side
// batch without telling the daemon, a cancel that lacks the request id (or the
// control token) and could hit another client's request, and pending entries
// that survive close().
import { getEventListeners } from "node:events";
import { test } from "node:test";
import assert from "node:assert/strict";

import {
  connect,
  RequestTimeoutError,
  TransportError,
  AbortError,
} from "../dist/index.js";
import { startFakeDaemon, stopFakeDaemon } from "./fake-daemon.mjs";

test("an aborted request leaves no abort listener behind", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  const ac = new AbortController();
  setTimeout(() => ac.abort(), 20);
  try {
    const err = await client.request("hang", undefined, { signal: ac.signal }).catch((e) => e);
    assert.ok(err instanceof AbortError);
    // The listener is removed when the request settles — an abort that fires
    // later must not run the settled request's handler.
    assert.equal(getEventListeners(ac.signal, "abort").length, 0);
    ac.abort(); // no-op now, must not throw
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("completed requests leave no abort listeners even at volume", async () => {
  // 200 sequential completions on one signal: any leak would pile up listeners
  // (the classic MaxListenersExceededWarning shape) and later aborts would
  // re-settle long-gone requests.
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  const ac = new AbortController();
  try {
    for (let i = 0; i < 200; i++) {
      const echoed = await client.request("echo", { i }, { signal: ac.signal });
      assert.deepEqual(echoed.echoed, { i });
    }
    assert.equal(getEventListeners(ac.signal, "abort").length, 0);
    ac.abort();
    assert.equal(getEventListeners(ac.signal, "abort").length, 0);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("a timed-out act sends a precise cancel and reports the confirmed ack", async () => {
  const fake = startFakeDaemon({ actDelayMs: 500 });
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const started = await client.session("start"); // issues the token
    const err = await client
      .act(
        { session_id: started.session_id, frame_id: "f1", actions: [{ type: "wait", duration_ms: 100_000 }] },
        { timeoutMs: 80 },
      )
      .catch((e) => e);
    assert.ok(err instanceof RequestTimeoutError, `expected RequestTimeoutError, got ${err}`);
    assert.equal(err.code, "REQUEST_TIMEOUT");
    // The daemon acknowledged the cancel → the SDK may truthfully claim the
    // runtime confirmed the cancellation.
    assert.equal(err.runtimeCancellationConfirmed, true);
    // The cancel carried session id + control token + the act's own request id
    // (3: id 1 was connect()'s version check, id 2 the session start), so it
    // can only target this act.
    const cancelReq = fake.requests.find((r) => r.method === "computer.cancel");
    assert.ok(cancelReq, "timeout must send computer.cancel");
    assert.equal(cancelReq.params.session_id, started.session_id);
    assert.equal(cancelReq.params.control_token, started.control_token);
    assert.equal(cancelReq.params.request_id, 3);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("a timed-out act reports unconfirmed when the daemon never acks", async () => {
  const fake = startFakeDaemon({ actDelayMs: 500, cancelAck: false });
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const started = await client.session("start");
    const err = await client
      .act(
        { session_id: started.session_id, frame_id: "f1", actions: [{ type: "wait", duration_ms: 100_000 }] },
        { timeoutMs: 80 },
      )
      .catch((e) => e);
    // cancelGracePeriodMs is a client option (default 1000ms) — the window
    // elapses while the fake stays mute on the cancel acknowledgement.
    assert.ok(err instanceof RequestTimeoutError);
    // The SDK must never claim the runtime stopped without proof.
    assert.equal(err.runtimeCancellationConfirmed, false);
    // The cancel was still sent; only the acknowledgement is missing.
    const cancelReq = fake.requests.find((r) => r.method === "computer.cancel");
    assert.ok(cancelReq, "timeout must still send computer.cancel");
    assert.equal(cancelReq.params.control_token, started.control_token);
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("only the timed-out client's act is cancelled, the other connection sails on", async () => {
  const fake = startFakeDaemon({ actDelayMs: 500 });
  const clientA = await connect({ socketPath: fake.socketPath });
  const clientB = await connect({ socketPath: fake.socketPath });
  try {
    // Both clients start their own sessions (each gets its own token) and act
    // at the same time. A's act times out; B's must not be touched.
    const [a, b] = await Promise.all([
      clientA.session("start"),
      clientB.session("start"),
    ]);
    const actA = clientA
      .act({ session_id: a.session_id, frame_id: "f1", actions: [{ type: "wait", duration_ms: 100_000 }] }, { timeoutMs: 60 })
      .catch((e) => e);
    const actB = clientB
      .act({ session_id: b.session_id, frame_id: "f1", actions: [{ type: "wait", duration_ms: 100_000 }] }, { timeoutMs: 2_000 })
      .catch((e) => e);
    const [errA, resB] = await Promise.all([actA, actB]);
    assert.ok(errA instanceof RequestTimeoutError, `A timed out: ${errA}`);
    // B's act completed normally — nothing about A's timeout touched B.
    assert.equal(resB.executed, true);
    assert.equal(resB.action_results[0].status, "success");
    // The only cancel the daemon received came from A's connection and named
    // A's request id (2 on that connection).
    const cancels = fake.requests.filter((r) => r.method === "computer.cancel");
    assert.equal(cancels.length, 1, "exactly one cancel, from the timed-out client");
    const conns = new Set(fake.requests.map((r) => r.conn));
    assert.equal(conns.size, 2, "two client connections");
    // A and B each issued act ids 3 (id 1 = version check, id 2 = session
    // start on each connection); the cancel must reference A's act id on A's
    // connection — the daemon keys the cancel by (connection_id, request_id),
    // so which connection sent it is exactly what isolates the two clients.
    const acts = fake.requests.filter((r) => r.method === "computer.act");
    assert.equal(acts.length, 2);
    assert.equal(cancels[0].params.request_id, 3);
    assert.ok(
      acts.some((r) => r.conn === cancels[0].conn && r.id === cancels[0].params.request_id),
      "cancel targets the same connection+request_id as A's act",
    );
  } finally {
    clientA.close();
    clientB.close();
    stopFakeDaemon(fake);
  }
});

test("two clients reusing the same request ids are isolated", async () => {
  // Both connections issue identical ids: version check 1, session start 2,
  // act 3. B's act completes after A's timeout; A's cancel (request_id 3 on
  // A's connection) must not disturb B's act with the same id.
  const fake = startFakeDaemon({ actDelayMs: 250 });
  const clientA = await connect({ socketPath: fake.socketPath });
  const clientB = await connect({ socketPath: fake.socketPath });
  try {
    const a = await clientA.session("start");
    const b = await clientB.session("start");
    const actA = clientA
      .act({ session_id: a.session_id, frame_id: "f1", actions: [{ type: "wait", duration_ms: 100_000 }] }, { timeoutMs: 50 })
      .catch((e) => e);
    // B's act lands 100ms later with the same numeric id on its own connection.
    await new Promise((r) => setTimeout(r, 100));
    const actB = clientB
      .act({ session_id: b.session_id, frame_id: "f1", actions: [{ type: "wait", duration_ms: 100_000 }] }, { timeoutMs: 2_000 })
      .catch((e) => e);
    const [errA, resB] = await Promise.all([actA, actB]);
    assert.ok(errA instanceof RequestTimeoutError);
    assert.equal(resB.executed, true);
    const cancelReq = fake.requests.find((r) => r.method === "computer.cancel");
    assert.ok(cancelReq, "A sent its precise cancel");
    assert.equal(cancelReq.params.request_id, 3);
    // B's act (same id 3, different connection) was never cancelled by name.
    const actsB = fake.requests.filter((r) => r.method === "computer.act" && r.conn !== cancelReq.conn);
    assert.equal(actsB.length, 1);
    assert.equal(actsB[0].id, 3, "B reused id 3 on its own connection");
  } finally {
    clientA.close();
    clientB.close();
    stopFakeDaemon(fake);
  }
});

test("close() rejects in-flight requests and subsequent requests refuse", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const inFlight = client.request("hang").catch((e) => e);
    client.close();
    const err = await inFlight;
    assert.ok(err instanceof TransportError);
    assert.match(err.message, /client closed/);
    // After close, the client refuses new work instead of pretending to send.
    const refused = await client.request("echo", {}).catch((e) => e);
    assert.ok(refused instanceof TransportError);
    assert.match(refused.message, /not connected/);
  } finally {
    stopFakeDaemon(fake);
  }
});

test("abort of a non-session request does not fabricate a cancel", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const ac = new AbortController();
    setTimeout(() => ac.abort(), 20);
    const err = await client.request("hang", undefined, { signal: ac.signal }).catch((e) => e);
    assert.ok(err instanceof AbortError);
    await new Promise((r) => setTimeout(r, 30));
    assert.equal(
      fake.requests.filter((r) => r.method === "computer.cancel").length,
      0,
      "no session id → no cancel notify",
    );
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("a stop drops the credential; a later ensureSession starts fresh", async () => {
  const fake = startFakeDaemon();
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const s1 = await client.ensureSession();
    assert.ok(client.getSessionCredential(), "credential held after start");
    assert.equal(client.getSessionCredential().sessionId, s1.session_id);
    await client.session("stop", { session_id: s1.session_id });
    assert.equal(client.getSessionCredential(), null, "credential dropped on stop");
    const s2 = await client.ensureSession();
    assert.notEqual(s2.session_id, s1.session_id, "a fresh session starts");
    assert.ok(client.getSessionCredential(), "new credential held");
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

// ---------------------------------------------------------------------------
// ensureSession policy matrix (§ 八): with an active session owned by another
// client, the SDK must never silently attach. "reject" surfaces the daemon's
// CONTROL_LOCKED (with the owner's identity), "read_only" returns the status
// without any credential, and "attach_with_token" adopts the session only
// with an explicitly provided capability.
// ---------------------------------------------------------------------------

const FOREIGN_SESSION = {
  session_id: "s_foreign",
  state: "active",
  paused: false,
  user_takeover: false,
  lock_held: true,
  display_id: "1",
  created_at: "2026-08-03T00:00:00Z",
  started_by: "OpenCode",
  owner_client_id: "opencode",
  owner_client_name: "OpenCode MCP",
  owner_instance_id: "opencode-inst-1",
};

test("ensureSession with the default reject policy surfaces CONTROL_LOCKED with the owner", async () => {
  const fake = startFakeDaemon({ seedSession: FOREIGN_SESSION, controlLockOnStart: true });
  const client = await connect({ socketPath: fake.socketPath });
  try {
    await assert.rejects(client.ensureSession(), (err) => {
      assert.equal(err.code, "CONTROL_LOCKED");
      // The wire message is the code; the non-secret owner identity rides in
      // data — a token never does.
      assert.equal(err.message, "CONTROL_LOCKED");
      assert.equal(err.data.holder, "s_foreign");
      assert.equal(err.data.owner.client_id, "opencode");
      assert.equal(err.data.owner.client_name, "OpenCode MCP");
      assert.equal(err.data.owner.client_instance_id, "opencode-inst-1");
      return true;
    });
    assert.equal(fake.state.startCount, 1, "reject probes with a start to surface the lock");
    assert.equal(fake.state.session.session_id, "s_foreign", "the foreign session was not disturbed");
    assert.equal(client.getSessionCredential(), null, "no credential after a rejected start");
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("ensureSession read_only returns the foreign session without a credential", async () => {
  const fake = startFakeDaemon({ seedSession: FOREIGN_SESSION });
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const s = await client.ensureSession(undefined, {}, undefined, "read_only");
    assert.equal(s.session_id, "s_foreign");
    assert.equal(client.getSessionCredential(), null, "read_only must not hold the token");
    assert.equal(fake.state.startCount, 0, "read_only never starts a session");
    // With no credential, an act is sent without a control_token — the daemon
    // refuses it (CONTROL_TOKEN_REQUIRED), which is the point of read_only.
    await client.act({ session_id: "s_foreign", frame_id: "f1", actions: [{ type: "wait", duration_ms: 1 }] });
    const actReq = fake.requests.find((r) => r.method === "computer.act");
    assert.ok(actReq, "act was attempted");
    assert.equal(actReq.params.control_token, undefined, "no credential → no token injected");
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});

test("ensureSession attach_with_token adopts the session with the caller's token", async () => {
  const fake = startFakeDaemon({ seedSession: FOREIGN_SESSION });
  const client = await connect({ socketPath: fake.socketPath });
  try {
    const s = await client.ensureSession(undefined, {}, undefined, "attach_with_token", "my-attach-token");
    assert.equal(s.session_id, "s_foreign");
    const cred = client.getSessionCredential();
    assert.ok(cred, "credential held after attach");
    assert.equal(cred.sessionId, "s_foreign");
    assert.equal(cred.controlToken, "my-attach-token");
    assert.equal(cred.ownerClientId, "opencode");
    assert.equal(cred.ownerInstanceId, "opencode-inst-1");
    assert.equal(fake.state.startCount, 0, "attach never starts a session");
    // The attached token is injected into later mutating calls.
    await client.act({ session_id: "s_foreign", frame_id: "f1", actions: [{ type: "wait", duration_ms: 1 }] });
    const actReq = fake.requests.find((r) => r.method === "computer.act");
    assert.equal(actReq.params.control_token, "my-attach-token");
    // An explicit token always wins over the injected one.
    await client.act(
      { session_id: "s_foreign", frame_id: "f1", actions: [{ type: "wait", duration_ms: 1 }], control_token: "explicit" },
    );
    const explicitReq = fake.requests.filter((r) => r.method === "computer.act").at(-1);
    assert.equal(explicitReq.params.control_token, "explicit");
    // stop clears the adopted credential.
    await client.session("stop", { session_id: "s_foreign" });
    assert.equal(client.getSessionCredential(), null, "stop clears the adopted credential");
  } finally {
    client.close();
    stopFakeDaemon(fake);
  }
});
