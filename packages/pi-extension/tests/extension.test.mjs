// Hermetic tests for the driver-mode extension: fake computer-use daemon on a
// temp Unix socket, stub DriverModel for the loop.
import { createServer } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import assert from "node:assert/strict";

import { ComputerUseExtension } from "../dist/index.js";

function startFakeDaemon({ staleFirstAct = false } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "cu-pi-test-"));
  const socketPath = join(dir, "fake.sock");
  const conns = new Set();
  let actCount = 0;
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
            result: { session_id: "s1", frame_id: `frame_${nextFrame()}`, width: 1440, height: 900, display_id: "1", scale_factor: 2, active_application: "FakeApp", image_path: "/tmp/f.png", image_mime_type: "image/png", captured_at: "2026-08-03T00:00:00Z" },
          });
        } else if (req.method === "computer.act") {
          actCount += 1;
          if (staleFirstAct && actCount === 1) {
            respond({
              jsonrpc: "2.0",
              id: req.id,
              error: { code: -32003, message: "STALE_FRAME", data: { code: "STALE_FRAME", message: "frame stale", referenced_frame_id: "frame_1", current_frame_id: "frame_2", change_score: 77, reason: "app_changed" } },
            });
          } else {
            respond({
              jsonrpc: "2.0",
              id: req.id,
              result: { executed: true, action_results: [{ index: 0, status: "success", duration_ms: 5 }], screen_changed: true, stable: true, next_frame_id: "frame_9" },
            });
          }
        } else if (req.method === "computer.inspect") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: { session_id: "s1", frame_id: "frame_1", width: 10, height: 10, image_base64: "AA==", image_mime_type: "image/png", mapping: { source_image_rect: { x: 0, y: 0, width: 10, height: 10, coordinate_space: "image_pixels" }, global_origin: [0, 0], normalized_1000_origin: [0, 0] } },
          });
        } else {
          respond({ jsonrpc: "2.0", id: req.id, result: null });
        }
      }
    });
  });
  let observeN = 0;
  function nextFrame() {
    observeN += 1;
    return observeN;
  }
  server.listen(socketPath);
  return { socketPath, server, dir, conns, actCount: () => actCount };
}

const stopFakeDaemon = (fake) => {
  for (const conn of fake.conns) conn.destroy();
  fake.server.close();
  rmSync(fake.dir, { recursive: true, force: true });
};

test("exposes the four tool schemas", async () => {
  const fake = startFakeDaemon();
  try {
    const ext = await ComputerUseExtension.create({ socketPath: fake.socketPath });
    const names = ext.toolSchemas().map((t) => t.name);
    assert.deepEqual(names.sort(), ["computer_act", "computer_inspect", "computer_observe", "computer_session"]);
    ext.close();
  } finally {
    stopFakeDaemon(fake);
  }
});

test("computer_observe auto-ensures a session and returns frame data", async () => {
  const fake = startFakeDaemon();
  try {
    const ext = await ComputerUseExtension.create({ socketPath: fake.socketPath });
    const result = await ext.handleTool("computer_observe");
    assert.equal(result.ok, true);
    assert.equal(result.data.frame_id, "frame_1");
    assert.equal(result.data.active_application, "FakeApp");
    ext.close();
  } finally {
    stopFakeDaemon(fake);
  }
});

test("computer_act executes and reports per-action results", async () => {
  const fake = startFakeDaemon();
  try {
    const ext = await ComputerUseExtension.create({ socketPath: fake.socketPath });
    await ext.handleTool("computer_observe");
    const result = await ext.handleTool("computer_act", {
      frame_id: "frame_1",
      actions: JSON.stringify([{ type: "move", x: 100, y: 100, coordinate_space: "normalized_1000" }]),
    });
    assert.equal(result.ok, true);
    assert.equal(result.data.action_results[0].status, "success");
    assert.equal(result.data.next_frame_id, "frame_9");
    ext.close();
  } finally {
    stopFakeDaemon(fake);
  }
});

test("computer_act rejects malformed actions JSON", async () => {
  const fake = startFakeDaemon();
  try {
    const ext = await ComputerUseExtension.create({ socketPath: fake.socketPath });
    const result = await ext.handleTool("computer_act", { frame_id: "f", actions: "oops" });
    assert.equal(result.ok, false);
    assert.equal(result.code, "INVALID_PARAMS");
    ext.close();
  } finally {
    stopFakeDaemon(fake);
  }
});

test("unknown tool is rejected", async () => {
  const fake = startFakeDaemon();
  try {
    const ext = await ComputerUseExtension.create({ socketPath: fake.socketPath });
    const result = await ext.handleTool("computer_explode");
    assert.equal(result.ok, false);
    assert.equal(result.code, "METHOD_NOT_FOUND");
    ext.close();
  } finally {
    stopFakeDaemon(fake);
  }
});

test("driver loop runs observe→act→done with history", async () => {
  const fake = startFakeDaemon();
  try {
    const ext = await ComputerUseExtension.create({ socketPath: fake.socketPath });
    const seen = [];
    const model = {
      async decide(ctx) {
        seen.push({ step: ctx.step, historyLength: ctx.history.length });
        if (ctx.step === 1) {
          return { kind: "act", actions: [{ type: "click", x: 1, y: 2, button: "left", coordinate_space: "normalized_1000" }] };
        }
        return { kind: "done", summary: "clicked" };
      },
    };
    const outcome = await ext.runDriverLoop(model, { maxSteps: 5 });
    assert.equal(outcome.completed, true);
    assert.equal(outcome.steps, 2);
    assert.equal(outcome.summary, "clicked");
    assert.equal(seen.length, 2);
    assert.equal(seen[0].historyLength, 0);
    assert.equal(seen[1].historyLength, 1);
    ext.close();
  } finally {
    stopFakeDaemon(fake);
  }
});

test("driver loop retries on STALE_FRAME", async () => {
  const fake = startFakeDaemon({ staleFirstAct: true });
  try {
    const ext = await ComputerUseExtension.create({ socketPath: fake.socketPath });
    let decideCalls = 0;
    const model = {
      async decide() {
        decideCalls += 1;
        return { kind: "act", actions: [{ type: "wait", duration_ms: 1 }] };
      },
    };
    const outcome = await ext.runDriverLoop(model, { maxSteps: 3 });
    // First act rejected (stale) → re-observe → second act succeeds.
    assert.equal(outcome.completed, false);
    assert.equal(outcome.reason, "max steps (3) reached");
    assert.ok(decideCalls >= 2, `model saw at least 2 frames, got ${decideCalls}`);
    ext.close();
  } finally {
    stopFakeDaemon(fake);
  }
});

test("driver loop stops on model error", async () => {
  const fake = startFakeDaemon();
  try {
    const ext = await ComputerUseExtension.create({ socketPath: fake.socketPath });
    const model = { async decide() { return { kind: "error", message: "cannot find the button" }; } };
    const outcome = await ext.runDriverLoop(model, { maxSteps: 5 });
    assert.equal(outcome.completed, false);
    assert.match(outcome.reason, /cannot find the button/);
    ext.close();
  } finally {
    stopFakeDaemon(fake);
  }
});
