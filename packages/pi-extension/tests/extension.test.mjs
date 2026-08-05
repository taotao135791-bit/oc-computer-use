// Hermetic tests for the Pi extension.
//
// We cannot run the real Pi runtime in CI, so we drive the extension exactly
// the way Pi does: call the default-export factory with a recording fake of
// the official ExtensionAPI, then invoke the registered tools/commands and
// assert on the real content blocks they produce against a fake daemon.
//
// The fake daemon is *stateful* (mirrors the real runtime's session machine):
// no session at boot, `start` creates one, `status` returns it or
// SESSION_NOT_FOUND, `stop` removes it, pause/resume/takeover/release mutate
// it. That lets the tests assert first-use auto-creation, ownership, and
// stop-on-shutdown semantics against observable daemon state.
import { createServer } from "node:net";
import { existsSync, mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import assert from "node:assert/strict";

import { extensionForMime } from "../dist/index.js";

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
// A 1x1 JPEG (magic bytes FF D8 FF).
const TINY_JPEG_B64 =
  "/9j/4AAQSkZJRgABAQEAYABgAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/wAALCAABAAEBAREA/8QAFAABAAAAAAAAAAAAAAAAAAAACf/EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AVN//2Q==";

// --- Fake computer-use daemon (stateful) ------------------------------------

const NOT_FOUND_ERROR = {
  code: -32009,
  message: "SESSION_NOT_FOUND",
  data: { code: "SESSION_NOT_FOUND", message: "No active computer-use session exists." },
};

const OBSERVATION_REQUIRED_ERROR = {
  code: -32024,
  message: "OBSERVATION_TOKEN_REQUIRED",
  data: {
    code: "OBSERVATION_TOKEN_REQUIRED",
    message:
      "This operation requires the session observation token (or its control token). A session id alone grants no observation permission.",
  },
};

/**
 * Start a fake daemon. `existingSession` seeds an active session owned by
 * another client (owner_client_id/name/instance_id). `jpeg` makes observe
 * return a JPEG instead of a PNG.
 */
function startFakeDaemon({ existingSession = null, jpeg = false } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "cu-pi-ext-test-"));
  const socketPath = join(dir, "fake.sock");
  const conns = new Set();
  const requests = [];
  const state = {
    session: existingSession,
    startCount: 0,
    startCalls: [],
    stopCount: 0,
    stopCalls: [],
  };

  const mkSession = (id, over = {}) => ({
    session_id: id,
    state: "active",
    paused: false,
    user_takeover: false,
    lock_held: true,
    display_id: "1",
    created_at: "2026-08-03T00:00:00Z",
    started_by: "pi-extension-test",
    ...over,
  });

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
        const rpcErr = (code, message) => ({
          jsonrpc: "2.0",
          id: req.id,
          error: {
            code: code === "SESSION_NOT_FOUND" ? -32009 : -32004,
            message: code,
            data: { code, message },
          },
        });
        // Mirrors the real daemon's CONTROL_LOCKED wire shape: message is the
        // code, and `data` carries the holder's non-secret identity.
        const controlLockedErr = () => ({
          jsonrpc: "2.0",
          id: req.id,
          error: {
            code: -32004,
            message: "CONTROL_LOCKED",
            data: {
              holder: state.session.session_id,
              owner: {
                client_id: state.session.owner_client_id,
                client_name: state.session.owner_client_name,
                client_instance_id: state.session.owner_instance_id,
              },
            },
          },
        });

        if (req.method === "computer.session") {
          const action = req.params?.action;
          if (action === "start") {
            if (state.session) {
              respond(controlLockedErr());
            } else {
              state.startCount += 1;
              state.startCalls.push(req.params);
              state.session = mkSession("s_started", {
                started_by: req.params?.client_name ?? "JSON-RPC client",
                owner_client_id: req.params?.client_id ?? "jsonrpc",
                owner_client_name: req.params?.client_name ?? "JSON-RPC client",
                owner_instance_id: req.params?.client_instance_id ?? "unknown",
              });
              // Both capability tokens appear exactly once — in the start
              // response. Status (below) never repeats either.
              respond({
                jsonrpc: "2.0",
                id: req.id,
                result: {
                  ...state.session,
                  control_token: "pi-fake-control-token",
                  observation_token: "pi-fake-observation-token",
                },
              });
            }
          } else if (action === "status") {
            // v3: status is a sensitive read — a session id alone grants no
            // observation permission.
            if (!state.session) {
              respond({ jsonrpc: "2.0", id: req.id, error: NOT_FOUND_ERROR });
            } else if (
              req.params?.observation_token !== "pi-fake-observation-token" &&
              req.params?.control_token !== "pi-fake-control-token"
            ) {
              respond({
                jsonrpc: "2.0",
                id: req.id,
                error: OBSERVATION_REQUIRED_ERROR,
              });
            } else {
              respond({ jsonrpc: "2.0", id: req.id, result: state.session });
            }
          } else if (action === "stop") {
            if (!state.session) respond({ jsonrpc: "2.0", id: req.id, error: NOT_FOUND_ERROR });
            else {
              state.stopCount += 1;
              state.stopCalls.push(req.params);
              const s = state.session;
              state.session = null;
              respond({
                jsonrpc: "2.0",
                id: req.id,
                result: { ...s, state: "stopped", lock_held: false },
              });
            }
          } else {
            if (!state.session) respond({ jsonrpc: "2.0", id: req.id, error: NOT_FOUND_ERROR });
            else {
              const s = state.session;
              if (action === "pause") Object.assign(s, { paused: true, state: "paused" });
              if (action === "resume") Object.assign(s, { paused: false, state: "active" });
              if (action === "takeover") Object.assign(s, { user_takeover: true, state: "user_takeover" });
              if (action === "release") Object.assign(s, { user_takeover: false, state: "active" });
              respond({ jsonrpc: "2.0", id: req.id, result: s });
            }
          }
        } else if (req.method === "session.summary") {
          // v3 public coarse view: tokenless. `null` session_id when none.
          const s = state.session;
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: {
              session_id: s?.session_id ?? null,
              state: s?.state ?? null,
              lock_held: s?.lock_held ?? false,
              owner_client_id: s?.owner_client_id ?? null,
              owner_client_name: s?.owner_client_name ?? null,
              message: s ? "knowing its id grants no observation or control permission" : null,
            },
          });
        } else if (req.method === "computer.observe") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: {
              session_id: req.params?.session_id ?? "s_active",
              frame_id: jpeg ? "frame_jpeg" : "frame_1",
              width: 1440,
              height: 900,
              display_id: "1",
              scale_factor: 2,
              active_application: "FakeApp",
              active_window: "Fake Window",
              image_base64: jpeg ? TINY_JPEG_B64 : TINY_PNG_B64,
              image_path: jpeg ? "/tmp/fake_frame_jpeg.jpg" : "/tmp/fake_frame_1.png",
              image_mime_type: jpeg ? "image/jpeg" : "image/png",
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
                session_id: req.params?.session_id ?? "s_active",
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
              active_sessions: state.session ? 1 : 0,
              uptime_secs: 42,
              frame_cache: 0,
            },
          });
        } else if (req.method === "runtime.version") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: { name: "fake", version: "0.1.0", protocol_version: 3 },
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
  return { socketPath, server, dir, conns, requests, state };
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
const cmdCtx = { cwd: "/tmp", ui: { notify: (msg, type) => notifications.push({ msg, type }) } };
const notifications = [];

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

test("extensionForMime maps MIME types to file extensions", () => {
  assert.equal(extensionForMime("image/png"), "png");
  assert.equal(extensionForMime("image/jpeg"), "jpg");
  assert.throws(() => extensionForMime("image/webp"), /unsupported image MIME type/);
});

test("first observe auto-creates exactly one session, owned by Pi", async () => {
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
    // First use: the tokenless summary probe found no session, then exactly
    // ONE start happened (v3: `status` is a sensitive read, so discovery
    // goes through the public session.summary).
    assert.equal(fake.state.startCount, 1, "exactly one session start on first use");
    const idxSummary = fake.requests.findIndex((r) => r.method === "session.summary");
    const idxStart = fake.requests.findIndex(
      (r) => r.method === "computer.session" && r.params?.action === "start",
    );
    assert.ok(idxSummary >= 0 && idxStart > idxSummary, "summary probed before start");
    // The start carried Pi's identity (the session's recorded owner).
    assert.equal(fake.state.startCalls[0].client_id, "pi-extension");
    assert.equal(fake.state.startCalls[0].client_name, "Pi");
    assert.match(fake.state.startCalls[0].client_instance_id, /^pi-\d+-[a-z0-9]{6}$/);
    assert.equal(fake.state.session.owner_client_id, "pi-extension");
    // The daemon was asked for the base64 image (vision-first).
    const observeReq = fake.requests.find((r) => r.method === "computer.observe");
    assert.equal(observeReq.params.include_image, true);
    // A second observe reuses the session — no second start.
    await observe.execute("call-1b", {}, undefined, undefined, ctx);
    assert.equal(fake.state.startCount, 1, "no second start for an existing session");
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});

test("observe rejects a session owned by another client with CONTROL_LOCKED", async () => {
  const fake = startFakeDaemon({
    existingSession: {
      session_id: "s_foreign",
      state: "active",
      paused: false,
      user_takeover: false,
      lock_held: true,
      display_id: "1",
      created_at: "2026-08-03T00:00:00Z",
      started_by: "OpenCode",
      owner_client_id: "opencode-mcp",
      owner_client_name: "OpenCode MCP",
      owner_instance_id: "opencode-instance-1",
    },
  });
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  try {
    const { pi, tools } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    const observe = tools.find((t) => t.name === "computer_observe");
    await assert.rejects(
      observe.execute("call-2", {}, undefined, undefined, ctx),
      (err) => {
        assert.equal(err.code, "CONTROL_LOCKED");
        // The daemon's wire message is the code itself; the identity of the
        // owner rides in `data` (non-secret) — never a token.
        assert.equal(err.message, "CONTROL_LOCKED");
        assert.equal(err.data.holder, "s_foreign");
        assert.equal(err.data.owner.client_name, "OpenCode MCP");
        assert.equal(err.data.owner.client_id, "opencode-mcp");
        assert.equal(err.data.owner.client_instance_id, "opencode-instance-1");
        return true;
      },
    );
    // Never started a competing session, never observed the foreign one.
    assert.equal(fake.state.startCount, 0);
    assert.equal(fake.requests.filter((r) => r.method === "computer.observe").length, 0);
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});

/**
 * A session owned by another client is never touched: the only existing
 * session policy is `reject`, so any tool call against a foreign session is
 * refused with the daemon's CONTROL_LOCKED (never a silent, tokenless
 * attach). `policyValue` (when given) sets the env var; removed policy
 * values must additionally warn.
 */
async function foreignSessionRefused(policyValue, expectDeprecationWarning) {
  const fake = startFakeDaemon({
    existingSession: {
      session_id: "s_foreign",
      state: "active",
      paused: false,
      user_takeover: false,
      lock_held: true,
      display_id: "1",
      created_at: "2026-08-03T00:00:00Z",
      started_by: "OpenCode",
      owner_client_id: "opencode-mcp",
      owner_client_name: "OpenCode MCP",
      owner_instance_id: "opencode-instance-1",
    },
  });
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  if (policyValue) process.env.COMPUTER_USE_EXISTING_SESSION_POLICY = policyValue;
  const warnings = [];
  const origWarn = console.warn;
  console.warn = (msg) => warnings.push(String(msg));
  try {
    const { pi, tools, handlers } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    const observe = tools.find((t) => t.name === "computer_observe");
    // The only policy is reject: the start attempt is refused by the daemon
    // with CONTROL_LOCKED, carrying the owner's non-secret identity.
    await assert.rejects(
      observe.execute("call-3", {}, undefined, undefined, ctx),
      (err) => {
        assert.equal(err.code, "CONTROL_LOCKED");
        return true;
      },
    );
    assert.equal(fake.state.startCount, 0, "reject never starts a session");
    // Shutdown must NOT stop a session we do not own.
    await handlers["session_shutdown"]();
    assert.equal(fake.state.stopCount, 0, "foreign session is not stopped on shutdown");
    return { warnings, fake };
  } finally {
    console.warn = origWarn;
    delete process.env.COMPUTER_USE_SOCKET;
    delete process.env.COMPUTER_USE_EXISTING_SESSION_POLICY;
    stopFakeDaemon(fake);
  }
}

test("a foreign session is refused with CONTROL_LOCKED — reject is the only policy", async () => {
  const { warnings } = await foreignSessionRefused(undefined, false);
  assert.equal(warnings.length, 0, "the default policy must not warn");
});

test("removed read_only policy value is deprecated and behaves like reject", async () => {
  const { warnings } = await foreignSessionRefused("read_only", true);
  assert.ok(
    warnings.some((w) => /read_only.*deprecated.*reject/.test(w)),
    `expected a deprecation warning, got: ${JSON.stringify(warnings)}`,
  );
});

test("legacy attach policy value is deprecated and behaves like reject", async () => {
  const { warnings } = await foreignSessionRefused("attach", true);
  assert.ok(
    warnings.some((w) => /attach.*deprecated.*reject/.test(w)),
    `expected a deprecation warning, got: ${JSON.stringify(warnings)}`,
  );
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
      "call-4",
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
      act.execute("call-5", { frame_id: "frame_1", actions: [{ type: "click", y: 10 }] }, undefined, undefined, ctx),
      /actions\[0\]: "click" requires x/,
    );
    await assert.rejects(
      act.execute("call-6", { frame_id: "frame_1", actions: [{ type: "wait" }] }, undefined, undefined, ctx),
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
      "call-7",
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
    const result = await act.execute("call-8", { frame_id: "frame_1", actions: [{ type: "wait", duration_ms: 1 }] }, ac.signal, undefined, ctx);
    const text = result.content.find((b) => b.type === "text");
    assert.match(text.text, /cancel/i);
    // Nothing reached the daemon.
    assert.equal(fake.requests.filter((r) => r.method === "computer.act").length, 0);
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});

test("session_shutdown stops only the session Pi created", async () => {
  const fake = startFakeDaemon();
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  try {
    const { pi, tools, handlers } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    const observe = tools.find((t) => t.name === "computer_observe");
    await observe.execute("call-9", {}, undefined, undefined, ctx);
    assert.equal(fake.state.startCount, 1, "observe auto-created the session");
    await handlers["session_shutdown"]();
    assert.equal(fake.state.stopCount, 1, "owned session is stopped on shutdown");
    assert.equal(fake.state.stopCalls[0].session_id, "s_started");
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
    notifications.length = 0;
    const status = commands.find((c) => c.name === "computer-status");
    // No session yet → "session: none".
    await status.handler("", cmdCtx);
    assert.equal(notifications.length, 1);
    assert.match(notifications[0].msg, /daemon: v0\.1\.0 ready/);
    assert.match(notifications[0].msg, /screen_recording=true accessibility=true/);
    assert.match(notifications[0].msg, /session: none/);
    // After /computer-start → the active session is reported.
    const start = commands.find((c) => c.name === "computer-start");
    await start.handler("", cmdCtx);
    notifications.length = 0;
    await status.handler("", cmdCtx);
    assert.match(notifications[0].msg, /session: s_started state=active/);
    assert.match(notifications[0].msg, /owner=Pi/);
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
    notifications.length = 0;
    const start = commands.find((c) => c.name === "computer-start");
    await start.handler("", cmdCtx);
    const takeover = commands.find((c) => c.name === "computer-takeover");
    await takeover.handler("", cmdCtx);
    const release = commands.find((c) => c.name === "computer-release");
    await release.handler("", cmdCtx);
    const actions = fake.requests.filter((r) => r.method === "computer.session").map((r) => r.params?.action);
    assert.ok(actions.includes("start"));
    assert.ok(actions.includes("takeover"));
    assert.ok(actions.includes("release"));
    assert.match(notifications[0].msg, /started: s_started state=active/);
    assert.match(notifications[1].msg, /takeover: s_started state=user_takeover paused=false takeover=true/);
    assert.match(notifications[2].msg, /released: s_started state=active paused=false takeover=false/);
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});

test("/computer-observe saves a PNG to the temp dir with 0600 and cleans it up", async () => {
  const fake = startFakeDaemon();
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  try {
    const { pi, commands, handlers } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    notifications.length = 0;
    const observeCmd = commands.find((c) => c.name === "computer-observe");
    await observeCmd.handler("", cmdCtx);
    const m = notifications[0].msg.match(/screenshot saved: (.+?) \(\d+x\d+/);
    assert.ok(m, `notification mentions the saved path: ${notifications[0].msg}`);
    const dest = m[1];
    // MIME-derived extension, system temp dir (never the project/cwd).
    assert.ok(dest.endsWith("oc-computer-use-s_started-frame_1.png"), `got ${dest}`);
    assert.ok(dest.startsWith(tmpdir()), `must be in the system temp dir, got ${dest}`);
    assert.equal(existsSync(join("/tmp", "oc-computer-use-s_started-frame_1.png")), false, "never written to the cwd");
    // Real PNG bytes, private to the current user.
    const buf = readFileSync(dest);
    assert.equal(buf.subarray(0, 8).toString("hex"), "89504e470d0a1a0a", "PNG magic bytes");
    assert.equal(statSync(dest).mode & 0o777, 0o600, "screenshot is user-private (0600)");
    // Shutdown removes the tracked temp file.
    await handlers["session_shutdown"]();
    assert.equal(existsSync(dest), false, "screenshot removed on shutdown");
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});

test("/computer-observe derives .jpg from a JPEG response", async () => {
  const fake = startFakeDaemon({ jpeg: true });
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  try {
    const { pi, commands, handlers } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    notifications.length = 0;
    const observeCmd = commands.find((c) => c.name === "computer-observe");
    await observeCmd.handler("", cmdCtx);
    const m = notifications[0].msg.match(/screenshot saved: (.+?) \(\d+x\d+/);
    assert.ok(m);
    const dest = m[1];
    assert.ok(dest.endsWith("oc-computer-use-s_started-frame_jpeg.jpg"), `got ${dest}`);
    const buf = readFileSync(dest);
    assert.equal(buf.subarray(0, 3).toString("hex"), "ffd8ff", "JPEG magic bytes");
    await handlers["session_shutdown"]();
    assert.equal(existsSync(dest), false);
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});

test("Pi tool params conform to the core protocol schema", async () => {
  // Pi's tool definitions are its own convenience layer; the params it sends
  // to the daemon must be schema-valid against the generated protocol (single
  // source of truth). Validate every recorded computer.* request with ajv
  // against the matching $defs entry.
  const { default: Ajv2019 } = await import("ajv/dist/2019.js");
  const { dirname } = await import("node:path");
  const { fileURLToPath } = await import("node:url");
  const schema = JSON.parse(
    readFileSync(
      join(dirname(fileURLToPath(import.meta.url)), "..", "..", "..", "protocol", "computer-use.schema.json"),
      "utf8",
    ),
  );
  const ajv = new Ajv2019({ strict: false });
  const validateAgainst = (defName) =>
    ajv.compile({
      $schema: "https://json-schema.org/draft/2019-09/schema",
      $ref: `#/$defs/${defName}`,
      $defs: schema.$defs,
    });

  const fake = startFakeDaemon();
  process.env.COMPUTER_USE_SOCKET = fake.socketPath;
  try {
    const { pi, tools } = fakePiApi();
    const createExtension = await loadExtension();
    createExtension(pi);
    const observe = tools.find((t) => t.name === "computer_observe");
    await observe.execute("call-9", {}, undefined, undefined, ctx);
    const act = tools.find((t) => t.name === "computer_act");
    await act.execute(
      "call-10",
      { frame_id: "frame_1", actions: [{ type: "click", x: 500, y: 500, button: "left" }] },
      undefined,
      undefined,
      ctx,
    );
    const inspect = tools.find((t) => t.name === "computer_inspect");
    await inspect.execute(
      "call-11",
      { frame_id: "frame_1", region: { x: 0, y: 0, width: 100, height: 100, coordinate_space: "image_pixels" } },
      undefined,
      undefined,
      ctx,
    );
    const session = tools.find((t) => t.name === "computer_session");
    await session.execute("call-12", { action: "status" }, undefined, undefined, ctx);

    const byDef = {
      "computer.session": "SessionParams",
      "computer.observe": "ObserveParams",
      "computer.act": "ActParams",
      "computer.inspect": "InspectParams",
    };
    const seen = new Set();
    for (const req of fake.requests) {
      const defName = byDef[req.method];
      if (!defName) continue;
      seen.add(req.method);
      const validateFn = validateAgainst(defName);
      const ok = validateFn(req.params);
      assert.ok(
        ok,
        `${req.method} params violate ${defName}: ${JSON.stringify(validateFn.errors)} — ${JSON.stringify(req.params)}`,
      );
    }
    assert.deepEqual([...seen].sort(), Object.keys(byDef).sort());
  } finally {
    delete process.env.COMPUTER_USE_SOCKET;
    stopFakeDaemon(fake);
  }
});
