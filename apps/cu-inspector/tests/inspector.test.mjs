// Hermetic test for the inspector HTTP server: fake daemon + real server.mjs.
import { createServer } from "node:net";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

// fileURLToPath (not .pathname): import.meta.url percent-encodes non-ASCII
// path segments (e.g. "测试"), which would make the spawn target unresolvable.
const SERVER_MJS = fileURLToPath(new URL("../server.mjs", import.meta.url));

function startFakeDaemon() {
  const dir = mkdtempSync(join(tmpdir(), "cu-insp-test-"));
  const socketPath = join(dir, "fake.sock");
  const framePath = join(dir, "frame_1.jpg");
  writeFileSync(framePath, Buffer.from("fakejpegbytes"));
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
        if (req.method === "runtime.version") {
          respond({ jsonrpc: "2.0", id: req.id, result: { name: "fake", version: "0.1.0", protocol_version: 3 } });
        } else if (req.method === "computer.session") {
          respond({ jsonrpc: "2.0", id: req.id, result: { session_id: "s1", state: "active", paused: false, user_takeover: false, lock_held: true, display_id: "1", created_at: "2026-08-03T00:00:00Z", started_by: "test", current_frame_id: "frame_1" } });
        } else if (req.method === "computer.observe") {
          respond({ jsonrpc: "2.0", id: req.id, result: { session_id: "s1", frame_id: "frame_1", width: 1440, height: 900, display_id: "1", scale_factor: 2, active_application: "FakeApp", image_path: framePath, image_mime_type: "image/jpeg", captured_at: "2026-08-03T00:00:00Z" } });
        } else if (req.method === "runtime.health") {
          respond({ jsonrpc: "2.0", id: req.id, result: { version: "0.1.0", ready: true, permissions: { screen_recording: true, accessibility: true }, active_sessions: 1, uptime_secs: 42, frame_cache: 3 } });
        } else if (req.method === "runtime.pointer") {
          respond({ jsonrpc: "2.0", id: req.id, result: { location: { x: 111, y: 222 }, display_id: "1" } });
        } else if (req.method === "trace.list") {
          respond({ jsonrpc: "2.0", id: req.id, result: { traces: [{ session_id: "s1", path: "/tmp/t.jsonl", entries: 5, bytes: 200, started_at: "2026-08-03T00:00:00Z" }] } });
        } else {
          respond({ jsonrpc: "2.0", id: req.id, result: null });
        }
      }
    });
  });
  server.listen(socketPath);
  return { socketPath, server, dir, conns, framePath };
}

const stopFakeDaemon = (fake) => {
  for (const conn of fake.conns) conn.destroy();
  fake.server.close();
  rmSync(fake.dir, { recursive: true, force: true });
};

function startInspector(fake) {
  const port = 18000 + Math.floor(Math.random() * 1000);
  const proc = spawn("node", [SERVER_MJS], {
    env: { ...process.env, COMPUTER_USE_SOCKET: fake.socketPath, CU_INSPECTOR_PORT: String(port) },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stderr = "";
  proc.stderr.on("data", (c) => (stderr += c));
  const base = `http://127.0.0.1:${port}`;
  return { proc, base, stderr: () => stderr };
}

async function waitForServer(base, attempts = 50) {
  for (let i = 0; i < attempts; i++) {
    try {
      const res = await fetch(`${base}/api/health`);
      if (res.ok) return;
    } catch {
      // not up yet
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error("inspector did not start");
}

test("dashboard endpoints proxy the daemon", async () => {
  const fake = startFakeDaemon();
  const inspector = startInspector(fake);
  try {
    await waitForServer(inspector.base);

    const health = await (await fetch(`${inspector.base}/api/health`)).json();
    assert.equal(health.version, "0.1.0");
    assert.equal(health.uptime_secs, 42);

    const session = await (await fetch(`${inspector.base}/api/session`)).json();
    assert.equal(session.state, "active");

    const frame = await (await fetch(`${inspector.base}/api/frame`)).json();
    assert.equal(frame.frame_id, "frame_1");
    assert.equal(frame.width, 1440);

    const img = await fetch(`${inspector.base}/api/frame-image?path=${encodeURIComponent(fake.framePath)}`);
    assert.equal(img.status, 200);
    assert.equal(await img.text(), "fakejpegbytes");

    const pointer = await (await fetch(`${inspector.base}/api/pointer`)).json();
    assert.equal(pointer.location.x, 111);

    const traces = await (await fetch(`${inspector.base}/api/traces`)).json();
    assert.equal(traces.traces.length, 1);

    const html = await (await fetch(`${inspector.base}/`)).text();
    assert.match(html, /computer-use inspector/);
  } finally {
    inspector.proc.kill();
    stopFakeDaemon(fake);
  }
});

test("missing image path is a 400", async () => {
  const fake = startFakeDaemon();
  const inspector = startInspector(fake);
  try {
    await waitForServer(inspector.base);
    const res = await fetch(`${inspector.base}/api/frame-image`);
    assert.equal(res.status, 400);
  } finally {
    inspector.proc.kill();
    stopFakeDaemon(fake);
  }
});
