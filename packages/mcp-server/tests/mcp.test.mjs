// End-to-end test of the MCP server: spawn `dist/index.js` with the SDK's
// stdio transport, speak MCP (Content-Length framing) to it, and drive a fake
// computer-use daemon over a temp Unix socket.
import { spawn } from "node:child_process";
import { createServer } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import assert from "node:assert/strict";

const DIST_INDEX = join(dirname(fileURLToPath(import.meta.url)), "..", "dist", "index.js");

// A 1x1 red PNG.
const TINY_PNG_B64 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

// --- Fake computer-use daemon (stateful) ------------------------------------
// Mirrors the real runtime's session machine: no session at boot, `start`
// creates one (recording the caller's identity as owner), `status` returns it
// or SESSION_NOT_FOUND, so first-use auto-creation is observable.

const NOT_FOUND_ERROR = {
  code: -32009,
  message: "SESSION_NOT_FOUND",
  data: { code: "SESSION_NOT_FOUND", message: "No active computer-use session exists." },
};

function startFakeDaemon() {
  const dir = mkdtempSync(join(tmpdir(), "cu-mcp-test-"));
  const socketPath = join(dir, "fake.sock");
  const conns = new Set();
  const requests = [];
  const state = { session: null, startCount: 0, startCalls: [] };
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
        requests.push(req);
        const respond = (obj) => conn.write(`${JSON.stringify(obj)}\n`);
        if (req.method === "computer.session") {
          const action = req.params?.action;
          if (action === "start") {
            state.startCount += 1;
            state.startCalls.push(req.params);
            state.session = {
              session_id: "s_started",
              state: "active",
              paused: false,
              user_takeover: false,
              lock_held: true,
              display_id: "1",
              created_at: "2026-08-03T00:00:00Z",
              started_by: req.params?.client_name ?? "JSON-RPC client",
              owner_client_id: req.params?.client_id ?? "jsonrpc",
              owner_client_name: req.params?.client_name ?? "JSON-RPC client",
              owner_instance_id: req.params?.client_instance_id ?? "unknown",
            };
            respond({ jsonrpc: "2.0", id: req.id, result: state.session });
          } else if (action === "status") {
            if (!state.session) respond({ jsonrpc: "2.0", id: req.id, error: NOT_FOUND_ERROR });
            else respond({ jsonrpc: "2.0", id: req.id, result: state.session });
          } else {
            if (!state.session) respond({ jsonrpc: "2.0", id: req.id, error: NOT_FOUND_ERROR });
            else respond({ jsonrpc: "2.0", id: req.id, result: state.session });
          }
        } else if (req.method === "computer.observe") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: {
              session_id: req.params?.session_id ?? "s_active",
              frame_id: "frame_1",
              width: 1440,
              height: 900,
              display_id: "1",
              scale_factor: 2,
              active_application: "FakeApp",
              image_base64: TINY_PNG_B64,
              image_path: "/tmp/fake_frame_1.png",
              image_mime_type: "image/png",
              captured_at: "2026-08-03T00:00:00Z",
            },
          });
        } else if (req.method === "computer.act") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: {
              executed: true,
              action_results: [
                { index: 0, status: "success", duration_ms: 12 },
              ],
              screen_changed: false,
              stable: true,
              next_frame_id: "frame_2",
            },
          });
        } else if (req.method === "computer.inspect") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: {
              session_id: req.params?.session_id ?? "s_active",
              frame_id: "frame_1",
              width: 100,
              height: 100,
              image_base64: TINY_PNG_B64,
              image_mime_type: "image/png",
              mapping: {
                source_image_rect: { x: 0, y: 0, width: 100, height: 100, coordinate_space: "image_pixels" },
                global_origin: [0, 0],
                normalized_1000_origin: [0, 0],
              },
            },
          });
        } else if (req.method === "runtime.health") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: { version: "0.1.0", ready: true, permissions: { screen_recording: true, accessibility: true }, active_sessions: 1, uptime_secs: 1, frame_cache: 0 },
          });
        } else if (req.method === "computer.cancel") {
          respond({ jsonrpc: "2.0", id: req.id, result: { cancelled: true, session_id: req.params?.session_id } });
        } else if (req.method === "trace.list") {
          respond({ jsonrpc: "2.0", id: req.id, result: { traces: [] } });
        } else {
          respond({ jsonrpc: "2.0", id: req.id, result: null });
        }
      }
    });
  });
  server.listen(socketPath);
  return { socketPath, server, dir, conns, requests, state };
}

const stopFakeDaemon = (fake) => {
  for (const conn of fake.conns) conn.destroy();
  fake.server.close();
  rmSync(fake.dir, { recursive: true, force: true });
};

// --- Minimal MCP-over-stdio client ------------------------------------------
// Note: @modelcontextprotocol/sdk >= 1.x frames stdio messages as
// newline-delimited JSON (no Content-Length headers).

class McpClient {
  constructor(proc) {
    this.proc = proc;
    this.buffer = "";
    this.nextId = 1;
    this.pending = new Map();
    proc.stdout.on("data", (chunk) => {
      this.buffer += chunk.toString("utf8");
      this.drain();
    });
  }

  send(obj) {
    this.proc.stdin.write(`${JSON.stringify(obj)}\n`);
  }

  request(method, params) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.send({ jsonrpc: "2.0", id, method, params });
    });
  }

  drain() {
    let nl;
    while ((nl = this.buffer.indexOf("\n")) >= 0) {
      const line = this.buffer.slice(0, nl).trim();
      this.buffer = this.buffer.slice(nl + 1);
      if (!line) continue;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        continue;
      }
      if (msg.id && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) reject(new Error(`${msg.error.code}: ${msg.error.message}`));
        else resolve(msg.result);
      }
    }
  }

  async initialize() {
    const result = await this.request("initialize", {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "cu-mcp-test", version: "0.0.1" },
    });
    this.send({ jsonrpc: "2.0", method: "notifications/initialized" });
    return result;
  }

  listTools() {
    return this.request("tools/list", {});
  }

  callTool(name, args) {
    return this.request("tools/call", { name, arguments: args });
  }
}

// --- Tests -------------------------------------------------------------------

const spawnServer = (fake) => {
  const proc = spawn("node", [DIST_INDEX], {
    env: { ...process.env, COMPUTER_USE_SOCKET: fake.socketPath },
    stdio: ["pipe", "pipe", "pipe"],
  });
  let stderr = "";
  proc.stderr.on("data", (c) => (stderr += c));
  return { proc, client: new McpClient(proc), stderr: () => stderr };
};

test("initializes and lists the seven tools", { timeout: 15000 }, async () => {
  const fake = startFakeDaemon();
  const { proc, client } = spawnServer(fake);
  try {
    const init = await client.initialize();
    assert.equal(init.serverInfo.name, "computer-use");
    const { tools } = await client.listTools();
    const names = tools.map((t) => t.name).sort();
    assert.deepEqual(names, [
      "computer_act",
      "computer_cancel",
      "computer_inspect",
      "computer_observe",
      "computer_session",
      "trace_get",
      "trace_list",
    ]);
  } finally {
    proc.kill();
    stopFakeDaemon(fake);
  }
});

test("computer_act and computer_inspect expose structured JSON schemas", { timeout: 15000 }, async () => {
  const fake = startFakeDaemon();
  const { proc, client } = spawnServer(fake);
  try {
    await client.initialize();
    const { tools } = await client.listTools();
    const act = tools.find((t) => t.name === "computer_act");
    assert.equal(act.inputSchema.properties.actions.type, "array", "actions must be a real array, not a JSON string");
    assert.ok(
      act.inputSchema.properties.actions.items.oneOf ?? act.inputSchema.properties.actions.items.anyOf,
      "action array items must carry a discriminated union schema",
    );
    assert.equal(act.inputSchema.properties.actions.minItems, 1);
    assert.equal(act.inputSchema.properties.actions.maxItems, 50);
    const inspect = tools.find((t) => t.name === "computer_inspect");
    assert.equal(inspect.inputSchema.properties.region.type, "object");
    assert.equal(inspect.inputSchema.properties.region.properties.width.minimum, 1);
  } finally {
    proc.kill();
    stopFakeDaemon(fake);
  }
});

test("computer_observe returns an image content block", { timeout: 15000 }, async () => {
  const fake = startFakeDaemon();
  const { proc, client } = spawnServer(fake);
  try {
    await client.initialize();
    const result = await client.callTool("computer_observe", { include_image: true });
    assert.equal(result.isError, undefined);
    const image = result.content.find((b) => b.type === "image");
    assert.ok(image, "expected an image content block");
    assert.equal(image.data, TINY_PNG_B64);
    assert.equal(image.mimeType, "image/png");
    const text = result.content.find((b) => b.type === "text");
    assert.match(text.text, /frame_id: frame_1/);
    assert.match(text.text, /active_application: FakeApp/);
  } finally {
    proc.kill();
    stopFakeDaemon(fake);
  }
});

test("computer_observe without include_image still returns the image by default", { timeout: 15000 }, async () => {
  const fake = startFakeDaemon();
  const { proc, client } = spawnServer(fake);
  try {
    await client.initialize();
    const result = await client.callTool("computer_observe", {});
    const image = result.content.find((b) => b.type === "image");
    assert.ok(image, "vision-first: image present even without include_image");
  } finally {
    proc.kill();
    stopFakeDaemon(fake);
  }
});

test("first computer_observe auto-creates exactly one session with the MCP identity", { timeout: 15000 }, async () => {
  const fake = startFakeDaemon();
  const { proc, client } = spawnServer(fake);
  try {
    await client.initialize();
    const result = await client.callTool("computer_observe", { include_image: true });
    assert.equal(result.isError, undefined);
    assert.equal(fake.state.startCount, 1, "exactly one session start on first observe");
    // The start carried the MCP server's identity — the recorded owner.
    assert.equal(fake.state.startCalls[0].client_id, "mcp-server");
    assert.equal(fake.state.startCalls[0].client_name, "Computer Use MCP");
    assert.match(fake.state.startCalls[0].client_instance_id, /^mcp-\d+-[a-z0-9]{6}$/);
    assert.equal(fake.state.session.owner_client_id, "mcp-server");
    // The observe ran against the auto-created session.
    const observeReq = fake.requests.find((r) => r.method === "computer.observe");
    assert.equal(observeReq.params.session_id, "s_started");
    // A second observe reuses the session — no second start.
    await client.callTool("computer_observe", { include_image: true });
    assert.equal(fake.state.startCount, 1, "no second start for an existing session");
  } finally {
    proc.kill();
    stopFakeDaemon(fake);
  }
});

test("computer_act accepts a structured action array (not a JSON string)", { timeout: 15000 }, async () => {
  const fake = startFakeDaemon();
  const { proc, client } = spawnServer(fake);
  try {
    await client.initialize();
    const result = await client.callTool("computer_act", {
      session_id: "s_active",
      frame_id: "frame_1",
      actions: [{ type: "move", x: 100, y: 100 }],
      wait_policy: "until_stable",
    });
    assert.equal(result.isError, undefined);
    const text = result.content.find((b) => b.type === "text").text;
    assert.match(text, /action\[0\]: success \(12ms\)/);
    assert.match(text, /next_frame_id: frame_2/);
  } finally {
    proc.kill();
    stopFakeDaemon(fake);
  }
});

test("computer_act validates every action shape with field-level errors", { timeout: 15000 }, async () => {
  const fake = startFakeDaemon();
  const { proc, client } = spawnServer(fake);
  try {
    await client.initialize();
    // Negative coordinate: zod rejects with a path that names the field.
    const badCoord = await client.callTool("computer_act", {
      session_id: "s_active",
      frame_id: "frame_1",
      actions: [{ type: "click", x: -5, y: 10 }],
    });
    assert.equal(badCoord.isError, true);
    const badCoordText = badCoord.content.find((b) => b.type === "text").text;
    assert.match(badCoordText, /actions\[0\]\.x/);

    // Unknown action type: discriminated union rejects it.
    const badType = await client.callTool("computer_act", {
      session_id: "s_active",
      frame_id: "frame_1",
      actions: [{ type: "swipe", x: 1, y: 1 }],
    });
    assert.equal(badType.isError, true);
    assert.match(badType.content.find((b) => b.type === "text").text, /actions\[0\]\.type/);

    // Empty batch: at least one action is required.
    const empty = await client.callTool("computer_act", {
      session_id: "s_active",
      frame_id: "frame_1",
      actions: [],
    });
    assert.equal(empty.isError, true);
    assert.match(empty.content.find((b) => b.type === "text").text, /actions/);
  } finally {
    proc.kill();
    stopFakeDaemon(fake);
  }
});

test("computer_session start then status round-trip", { timeout: 15000 }, async () => {
  const fake = startFakeDaemon();
  const { proc, client } = spawnServer(fake);
  try {
    await client.initialize();
    // No session yet: status is a typed SESSION_NOT_FOUND error.
    const none = await client.callTool("computer_session", { action: "status" });
    assert.equal(none.isError, true);
    assert.match(none.content[0].text, /SESSION_NOT_FOUND/);
    // Start creates the session (with the MCP server as recorded owner).
    const started = await client.callTool("computer_session", { action: "start" });
    assert.match(started.content[0].text, /session_id: s_started/);
    assert.match(started.content[0].text, /state: active/);
    assert.equal(fake.state.startCount, 1);
    // Status now resolves it.
    const status = await client.callTool("computer_session", { action: "status" });
    assert.match(status.content[0].text, /session_id: s_started/);
  } finally {
    proc.kill();
    stopFakeDaemon(fake);
  }
});

test("computer_inspect accepts a structured region object and returns image plus mapping", { timeout: 15000 }, async () => {
  const fake = startFakeDaemon();
  const { proc, client } = spawnServer(fake);
  try {
    await client.initialize();
    const result = await client.callTool("computer_inspect", {
      session_id: "s_active",
      frame_id: "frame_1",
      region: {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
        coordinate_space: "normalized_1000",
      },
      scale: 4,
    });
    assert.equal(result.isError, undefined);
    const image = result.content.find((b) => b.type === "image");
    assert.ok(image);
    assert.match(result.content[0].text, /global_origin: 0,0/);
  } finally {
    proc.kill();
    stopFakeDaemon(fake);
  }
});

test("computer_inspect rejects an invalid region with a field-level error", { timeout: 15000 }, async () => {
  const fake = startFakeDaemon();
  const { proc, client } = spawnServer(fake);
  try {
    await client.initialize();
    const result = await client.callTool("computer_inspect", {
      session_id: "s_active",
      frame_id: "frame_1",
      region: { x: 0, y: 0, width: 0, height: 100 }, // width must be >= 1
    });
    assert.equal(result.isError, true);
    assert.match(result.content.find((b) => b.type === "text").text, /region/);
  } finally {
    proc.kill();
    stopFakeDaemon(fake);
  }
});
