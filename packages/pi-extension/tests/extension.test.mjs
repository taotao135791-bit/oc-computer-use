// Hermetic tests for the Pi extension.
//
// We cannot run the real Pi runtime in CI, so we drive the extension exactly
// the way Pi does: call the default-export factory with a recording fake of
// the official ExtensionAPI, then invoke the registered tools/commands and
// assert on the real content blocks they produce against a fake daemon.
import { createServer } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import assert from "node:assert/strict";

// The extension keeps module-scoped daemon state (client/session), so each
// test loads a fresh module instance (query string busts the ESM cache).
let extSeq = 0;
async function loadExtension() {
  extSeq += 1;
  const mod = await import(`../dist/index.js?t=${extSeq}`);
  return mod.default;
}

// A 1x1 red PNG.
const TINY_PNG_B64 =
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

// --- Fake computer-use daemon ----------------------------------------------

function startFakeDaemon() {
  const dir = mkdtempSync(join(tmpdir(), "cu-pi-ext-test-"));
  const socketPath = join(dir, "fake.sock");
  const conns = new Set();
  const requests = [];
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
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: {
              session_id: action === "start" ? "s_started" : "s_active",
              state: action === "stop" ? "stopped" : "active",
              paused: false,
              user_takeover: false,
              lock_held: action === "stop" ? false : true,
              display_id: "1",
              created_at: "2026-08-03T00:00:00Z",
              started_by: "pi-extension-test",
            },
          });
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
              active_window: "Fake Window",
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
              action_results: [{ index: 0, status: "success", duration_ms: 12 }],
              screen_changed: true,
              stable: true,
              next_frame_id: "frame_2",
              screenshot: {
                session_id: "s_active",
                frame_id: "frame_2",
                width: 1440,
                height: 900,
                display_id: "1",
                scale_factor: 2,
                image_base64: TINY_PNG_B64,
                image_path: "/tmp/fake_frame_2.png",
                image_mime_type: "image/png",
                captured_at: "2026-08-03T00:00:00Z",
              },
            },
          });
        } else if (req.method === "computer.inspect") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: {
              session_id: "s_active",
              frame_id: "frame_1",
              width: 100,
              height: 100,
              image_base64: TINY_PNG_B64,
              image_mime_type: "image/png",
              mapping: {
                source_image_rect: { x: 0, y: 0, width: 100, height: 100, coordinate_space: "image_pixels" },
                global_origin: [10, 20],
                normalized_1000_origin: [7, 22],
              },
            },
          });
        } else if (req.method === "runtime.health") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: {
              version: "0.1.0",
              ready: true,
              permissions: { screen_recording: true, accessibility: true },
              active_sessions: 1,
              uptime_secs: 42,
              frame_cache: 0,
            },
          });
        } else if (req.method === "hang") {
          // Never respond — used to exercise in-flight aborts.
        } else {
          respond({ jsonrpc: "2.0", id: req.id, result: null });
        }
      }
    });
  });
  server.listen(socketPath);
  return { socketPath, server, dir, conns, requests };
}

const stopFakeDaemon = (fake) => {
  for (const conn of fake.conns) conn.destroy();
  fake.server.close();
  rmSync(fake.dir, { recursive: true, force: true });
};

// --- Fake Pi ExtensionAPI --------------------------------------------------

function fakePiApi() {
  const tools = [];
  const commands = [];
  const handlers = {};
  return {
    pi: {
      registerTool: (def) => tools.push(def),
      registerCommand: (name, def) => commands.push({ name, ...def }),
      on: (event, handler) => {
        handlers[event] = handler;
      },
    },
    tools,
    commands,
    handlers,
  };
}

const ctx = { cwd: "/tmp" };

// --- Tests -----------------------------------------------------------------

test("registers the four tools, eight commands, and a session_shutdown handler", async () => {
  const { pi, tools, commands, handlers } = fakePiApi();
  const createExtension = await loadExtension();
  createExtension(pi);
  const toolNames = tools.map((t) => t.name).sort();
  assert.deepEqual(toolNames, [
    "computer_act",
    "computer_inspect",
    "computer_observe",
    "computer_session",
  ]);
  const commandNames = commands.map((c) => c.name).sort();
  assert.ok(commandNames.length >= 7, `expected >=7 commands, got ${commandNames.length}`);
  for (const required of ["computer-status", "computer-start", "computer-stop", "computer-pause", "computer-resume", "computer-takeover", "computer-release", "computer-observe"]) {
    assert.ok(commandNames.includes(required), `missing command ${required}`);
  }
  assert.equal(typeof handlers["session_shutdown"], "function");
});

test("computer_observe returns a real image content block plus text metadata", async () => {
  const fake = startFakeDaemon();
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  try {
    const { pi, tools } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    const observe = tools.find((t) => t.name === "computer_observe");
    const result = await observe.execute("call-1", {}, undefined, undefined, ctx);
    const image = result.content.find((b) => b.type === "image");
    assert.ok(image, "expected an image content block");
    assert.equal(image.data, TINY_PNG_B64);
    assert.equal(image.mimeType, "image/png");
    const text = result.content.find((b) => b.type === "text");
    assert.match(text.text, /frame_id: frame_1/);
    assert.match(text.text, /active_application: FakeApp/);
    assert.match(text.text, /size: 1440x900/);
    // The daemon was asked for the base64 image (vision-first).
    const observeReq = fake.requests.find((r) => r.method === "computer.observe");
    assert.equal(observeReq.params.include_image, true);
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});

test("computer_act executes and returns per-action results with the post-batch screenshot", async () => {
  const fake = startFakeDaemon();
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  try {
    const { pi, tools } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    const act = tools.find((t) => t.name === "computer_act");
    const result = await act.execute(
      "call-2",
      {
        frame_id: "frame_1",
        actions: [{ type: "click", x: 500, y: 500, button: "left" }],
        wait_policy: "until_stable",
      },
      undefined,
      undefined,
      ctx,
    );
    const text = result.content.find((b) => b.type === "text");
    assert.match(text.text, /action\[0\]: success \(12ms\)/);
    assert.match(text.text, /next_frame_id: frame_2/);
    const image = result.content.find((b) => b.type === "image");
    assert.ok(image, "expected the post-batch screenshot image block");
    assert.equal(image.data, TINY_PNG_B64);
    // The batch reached the daemon as structured objects (not a JSON string).
    const actReq = fake.requests.find((r) => r.method === "computer.act");
    assert.deepEqual(actReq.params.actions, [{ type: "click", x: 500, y: 500, button: "left", coordinate_space: "normalized_1000" }]);
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});

test("computer_act validates required fields with field-level errors", async () => {
  const fake = startFakeDaemon();
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  try {
    const { pi, tools } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    const act = tools.find((t) => t.name === "computer_act");
    await assert.rejects(
      act.execute("call-3", { frame_id: "frame_1", actions: [{ type: "click", y: 10 }] }, undefined, undefined, ctx),
      /actions\[0\]: "click" requires x/,
    );
    await assert.rejects(
      act.execute("call-4", { frame_id: "frame_1", actions: [{ type: "wait" }] }, undefined, undefined, ctx),
      /actions\[0\]: "wait" requires duration_ms/,
    );
    // No request should have reached the daemon for the invalid batch.
    assert.equal(fake.requests.filter((r) => r.method === "computer.act").length, 0);
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});

test("computer_inspect returns the crop as an image block plus mapping text", async () => {
  const fake = startFakeDaemon();
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  try {
    const { pi, tools } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    const inspect = tools.find((t) => t.name === "computer_inspect");
    const result = await inspect.execute(
      "call-5",
      { frame_id: "frame_1", region: { x: 700, y: 0, width: 300, height: 300 } },
      undefined,
      undefined,
      ctx,
    );
    const image = result.content.find((b) => b.type === "image");
    assert.ok(image);
    assert.equal(image.data, TINY_PNG_B64);
    const text = result.content.find((b) => b.type === "text");
    assert.match(text.text, /global_origin: 10,20/);
    assert.match(text.text, /normalized_1000_origin: 7,22/);
    const inspectReq = fake.requests.find((r) => r.method === "computer.inspect");
    assert.equal(inspectReq.params.region.coordinate_space, "normalized_1000");
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});

test("a pre-aborted signal cancels the tool call", async () => {
  const fake = startFakeDaemon();
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  try {
    const { pi, tools } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    const ac = new AbortController();
    ac.abort();
    const act = tools.find((t) => t.name === "computer_act");
    const result = await act.execute("call-6", { frame_id: "frame_1", actions: [{ type: "wait", duration_ms: 1 }] }, ac.signal, undefined, ctx);
    const text = result.content.find((b) => b.type === "text");
    assert.match(text.text, /cancel/i);
    // Nothing reached the daemon.
    assert.equal(fake.requests.filter((r) => r.method === "computer.act").length, 0);
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});

test("session_shutdown stops the session and closes the connection", async () => {
  const fake = startFakeDaemon();
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  try {
    const { pi, tools, handlers } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    const observe = tools.find((t) => t.name === "computer_observe");
    await observe.execute("call-7", {}, undefined, undefined, ctx);
    assert.ok(fake.requests.some((r) => r.method === "computer.observe"), "observe ran first");
    await handlers["session_shutdown"]();
    const stopReq = fake.requests.find((r) => r.method === "computer.session" && r.params?.action === "stop");
    assert.ok(stopReq, "expected a computer.session stop request on shutdown");
    assert.equal(stopReq.params.session_id, "s_active");
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});

test("/computer-status reports real daemon health and session state", async () => {
  const fake = startFakeDaemon();
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  try {
    const { pi, commands } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    const notifications = [];
    const cmdCtx = { cwd: "/tmp", ui: { notify: (msg, type) => notifications.push({ msg, type }) } };
    const status = commands.find((c) => c.name === "computer-status");
    await status.handler("", cmdCtx);
    assert.equal(notifications.length, 1);
    assert.match(notifications[0].msg, /daemon: v0\.1\.0 ready/);
    assert.match(notifications[0].msg, /screen_recording=true accessibility=true/);
    assert.match(notifications[0].msg, /session: s_active state=active/);
    assert.equal(notifications[0].type, "info");
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});

test("/computer-takeover and /computer-release round-trip through the daemon", async () => {
  const fake = startFakeDaemon();
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  try {
    const { pi, commands } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    const notifications = [];
    const cmdCtx = { cwd: "/tmp", ui: { notify: (msg, type) => notifications.push({ msg, type }) } };
    const takeover = commands.find((c) => c.name === "computer-takeover");
    await takeover.handler("", cmdCtx);
    const release = commands.find((c) => c.name === "computer-release");
    await release.handler("", cmdCtx);
    const actions = fake.requests.filter((r) => r.method === "computer.session").map((r) => r.params?.action);
    assert.ok(actions.includes("takeover"));
    assert.ok(actions.includes("release"));
    assert.match(notifications[0].msg, /takeover: s_active state=active/);
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});
