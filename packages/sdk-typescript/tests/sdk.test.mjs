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
        } else if (req.method === "hang") {
          // Never respond.
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
  assert.equal(errorCodeName(-32700), "PARSE_ERROR");
  assert.equal(errorCodeName(-9999), ERROR_CODES.INTERNAL);
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
