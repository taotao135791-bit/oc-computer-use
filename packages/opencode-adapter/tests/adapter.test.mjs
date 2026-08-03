// Hermetic tests for the OpenCode adapter: drive the plugin's tool functions
// against a fake computer-use daemon on a temp Unix socket.
import { createServer } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import assert from "node:assert/strict";

import { computerUsePlugin } from "../dist/index.js";

const TINY_PNG_B64 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

function startFakeDaemon() {
  const dir = mkdtempSync(join(tmpdir(), "cu-oc-test-"));
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
        if (req.method === "computer.session") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: { session_id: "s1", state: "active", paused: false, user_takeover: false, lock_held: true, display_id: "1", created_at: "2026-08-03T00:00:00Z", started_by: "test" },
          });
        } else if (req.method === "computer.observe") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: { session_id: "s1", frame_id: "frame_1", width: 1440, height: 900, display_id: "1", scale_factor: 2, active_application: "FakeApp", image_path: "/tmp/f1.png", image_mime_type: "image/png", captured_at: "2026-08-03T00:00:00Z" },
          });
        } else if (req.method === "computer.act") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: { executed: true, action_results: [{ index: 0, status: "success", duration_ms: 3 }], screen_changed: true, stable: true, next_frame_id: "frame_2" },
          });
        } else if (req.method === "computer.inspect") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: { session_id: "s1", frame_id: "frame_1", width: 50, height: 50, image_base64: TINY_PNG_B64, image_mime_type: "image/png", mapping: { source_image_rect: { x: 0, y: 0, width: 50, height: 50, coordinate_space: "image_pixels" }, global_origin: [10, 20], normalized_1000_origin: [3.5, 4.5] } },
          });
        } else if (req.method === "runtime.health") {
          respond({ jsonrpc: "2.0", id: req.id, result: { version: "0.1.0", ready: true, permissions: { screen_recording: true, accessibility: true }, active_sessions: 1, uptime_secs: 1, frame_cache: 0 } });
        } else {
          respond({ jsonrpc: "2.0", id: req.id, result: null });
        }
      }
    });
  });
  server.listen(socketPath);
  return { socketPath, server, dir, conns };
}

const stopFakeDaemon = (fake) => {
  for (const conn of fake.conns) conn.destroy();
  fake.server.close();
  rmSync(fake.dir, { recursive: true, force: true });
};

test("plugin exposes the four core tools", async () => {
  const fake = startFakeDaemon();
  try {
    const plugin = computerUsePlugin({ socketPath: fake.socketPath });
    const { tools } = await plugin({}, null);
    assert.deepEqual(Object.keys(tools).sort(), [
      "computer_act",
      "computer_inspect",
      "computer_observe",
      "computer_session",
    ]);
  } finally {
    stopFakeDaemon(fake);
  }
});

test("computer_observe returns frame metadata", async () => {
  const fake = startFakeDaemon();
  try {
    const plugin = computerUsePlugin({ socketPath: fake.socketPath });
    const { tools } = await plugin({}, null);
    const result = await tools.computer_observe.run({}, null);
    assert.equal(result.frame_id, "frame_1");
    assert.equal(result.width, 1440);
    assert.equal(result.active_application, "FakeApp");
    assert.equal(result.image_path, "/tmp/f1.png");
  } finally {
    stopFakeDaemon(fake);
  }
});

test("computer_act parses actions and reports results", async () => {
  const fake = startFakeDaemon();
  try {
    const plugin = computerUsePlugin({ socketPath: fake.socketPath });
    const { tools } = await plugin({}, null);
    const result = await tools.computer_act.run({
      session_id: "s1",
      frame_id: "frame_1",
      actions: JSON.stringify([{ type: "click", x: 10, y: 10, button: "left", coordinate_space: "normalized_1000" }]),
    }, null);
    assert.equal(result.executed, true);
    assert.equal(result.action_results[0].status, "success");
    assert.equal(result.next_frame_id, "frame_2");
  } finally {
    stopFakeDaemon(fake);
  }
});

test("computer_act rejects malformed JSON", async () => {
  const fake = startFakeDaemon();
  try {
    const plugin = computerUsePlugin({ socketPath: fake.socketPath });
    const { tools } = await plugin({}, null);
    const result = await tools.computer_act.run({ session_id: "s1", frame_id: "frame_1", actions: "nope" }, null);
    assert.match(String(result), /INVALID_PARAMS/);
  } finally {
    stopFakeDaemon(fake);
  }
});

test("computer_inspect returns mapping", async () => {
  const fake = startFakeDaemon();
  try {
    const plugin = computerUsePlugin({ socketPath: fake.socketPath });
    const { tools } = await plugin({}, null);
    const result = await tools.computer_inspect.run({
      session_id: "s1",
      frame_id: "frame_1",
      x: 0,
      y: 0,
      width: 50,
      height: 50,
    }, null);
    assert.equal(result.mapping.global_origin[0], 10);
    assert.equal(result.width, 50);
  } finally {
    stopFakeDaemon(fake);
  }
});

test("computer_session status round-trips", async () => {
  const fake = startFakeDaemon();
  try {
    const plugin = computerUsePlugin({ socketPath: fake.socketPath });
    const { tools } = await plugin({}, null);
    const result = await tools.computer_session.run({ action: "status" }, null);
    assert.equal(result.session_id, "s1");
    assert.equal(result.state, "active");
  } finally {
    stopFakeDaemon(fake);
  }
});
