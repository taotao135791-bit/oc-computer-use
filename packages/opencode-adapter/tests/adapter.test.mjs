// Hermetic tests for the OpenCode companion CLI.
//
// OpenCode consumes the runtime through the MCP server (not a plugin), so
// these tests drive the library functions and CLI `main` against a fake
// computer-use daemon on a temp Unix socket: config generation, status
// reporting, session cleanup, and doctor checks.
import { createServer } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import assert from "node:assert/strict";

import {
  cleanupSession,
  defaultOpenCodeConfigPath,
  generateOpenCodeConfig,
  statusText,
  writeOpenCodeConfig,
  doctor,
  doctorText,
} from "../dist/index.js";
import { main } from "../dist/cli.js";

/**
 * Fake daemon. `sessionHandler` (optional) receives the request and returns
 * the JSON-RPC result object; the default returns an active session.
 */
function startFakeDaemon({ sessionHandler } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "cu-oc-test-"));
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
          respond(sessionHandler?.(req) ?? {
            jsonrpc: "2.0",
            id: req.id,
            result: {
              session_id: "s1",
              state: req.params?.action === "stop" ? "stopped" : "active",
              paused: false,
              user_takeover: false,
              lock_held: req.params?.action !== "stop",
              display_id: "1",
              created_at: "2026-08-03T00:00:00Z",
              started_by: "opencode-adapter-test",
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

const connect = async (socketPath) => {
  const { connect: sdkConnect } = await import("@computer-use/sdk");
  return sdkConnect({ socketPath });
};

// Capture stdout while running the CLI `main`.
async function runMain(args) {
  const chunks = [];
  const orig = process.stdout.write;
  process.stdout.write = (chunk) => {
    chunks.push(String(chunk));
    return true;
  };
  try {
    await main(args);
  } finally {
    process.stdout.write = orig;
  }
  return chunks.join("");
}

test("generateOpenCodeConfig emits the official local MCP format", () => {
  const config = generateOpenCodeConfig();
  assert.deepEqual(config.mcp["computer-use"], {
    type: "local",
    command: ["computer-use-mcp"],
    enabled: true,
  });
  // The config schema URL is preserved when merging an existing config.
  const merged = generateOpenCodeConfig({
    $schema: "https://opencode.ai/config.json",
    mcp: { "some-other-server": { type: "local", command: ["other"], enabled: true } },
  });
  assert.equal(merged.$schema, "https://opencode.ai/config.json");
  assert.deepEqual(merged.mcp["some-other-server"], { type: "local", command: ["other"], enabled: true });
  // An existing computer-use entry is replaced (not duplicated).
  const replaced = generateOpenCodeConfig({ mcp: { "computer-use": { type: "stdio", command: ["old"] } } });
  assert.deepEqual(replaced.mcp["computer-use"], { type: "local", command: ["computer-use-mcp"], enabled: true });
});

test("writeOpenCodeConfig writes and merges into a real file", async () => {
  const dir = mkdtempSync(join(tmpdir(), "cu-oc-config-"));
  try {
    const path = join(dir, "opencode.json");
    const first = await writeOpenCodeConfig(path);
    assert.equal(first.existed, false);
    assert.equal(first.merged, false);
    const written = JSON.parse(await readFile(path, "utf8"));
    assert.deepEqual(written.mcp["computer-use"].command, ["computer-use-mcp"]);

    // Merging keeps an unrelated key and reports the replacement.
    await writeFile(path, JSON.stringify({ tools: { foo: true } }), "utf8");
    const second = await writeOpenCodeConfig(path);
    assert.equal(second.existed, true);
    assert.equal(second.merged, false);
    const merged = JSON.parse(await readFile(path, "utf8"));
    assert.equal(merged.tools.foo, true);
    assert.ok(merged.mcp["computer-use"]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("statusText reports real daemon health and session state", async () => {
  const fake = startFakeDaemon();
  try {
    const client = await connect(fake.socketPath);
    const text = await statusText(client);
    client.close();
    assert.match(text, /daemon: v0\.1\.0 ready/);
    assert.match(text, /screen_recording=true accessibility=true/);
    assert.match(text, /active_sessions: 1/);
    assert.match(text, /session: s1 state=active paused=false takeover=false lock=true/);
  } finally {
    stopFakeDaemon(fake);
  }
});

test("statusText reports \"session: none\" when no session exists", async () => {
  const fake = startFakeDaemon({
    sessionHandler: (req) => ({
      jsonrpc: "2.0",
      id: req.id,
      error: { code: -32000, message: "no active session", data: { code: "SESSION_NOT_FOUND" } },
    }),
  });
  try {
    const client = await connect(fake.socketPath);
    const text = await statusText(client);
    client.close();
    assert.match(text, /session: none/);
  } finally {
    stopFakeDaemon(fake);
  }
});

test("cleanupSession stops the active session", async () => {
  const fake = startFakeDaemon();
  try {
    const client = await connect(fake.socketPath);
    const result = await cleanupSession(client);
    client.close();
    assert.equal(result.stopped, true);
    assert.match(result.message, /stopped session s1 \(was active\)/);
    const stopReqs = fake.requests.filter((r) => r.method === "computer.session" && r.params?.action === "stop");
    assert.equal(stopReqs.length, 1);
    assert.equal(stopReqs[0].params.session_id, "s1");
  } finally {
    stopFakeDaemon(fake);
  }
});

test("cleanupSession is a no-op when no session exists", async () => {
  const fake = startFakeDaemon({
    sessionHandler: (req) => ({
      jsonrpc: "2.0",
      id: req.id,
      error: { code: -32000, message: "no active session", data: { code: "SESSION_NOT_FOUND" } },
    }),
  });
  try {
    const client = await connect(fake.socketPath);
    const result = await cleanupSession(client);
    client.close();
    assert.equal(result.stopped, false);
    assert.match(result.message, /no active session/);
    assert.equal(fake.requests.filter((r) => r.params?.action === "stop").length, 0);
  } finally {
    stopFakeDaemon(fake);
  }
});

test("doctor detects a missing socket and missing binaries", async () => {
  const dir = mkdtempSync(join(tmpdir(), "cu-oc-doctor-"));
  try {
    const socketPath = join(dir, "dead.sock");
    const report = await doctor({ socketPath });
    assert.equal(report.socket_exists, false);
    assert.equal(report.daemon_health, null);
    assert.ok(report.errors.some((e) => /no socket/.test(e)));
    assert.match(doctorText(report), /daemon: *unreachable/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("doctor reports health against a live fake daemon", async () => {
  const fake = startFakeDaemon();
  try {
    const report = await doctor({ socketPath: fake.socketPath });
    assert.equal(report.socket_exists, true);
    assert.equal(report.daemon_health.version, "0.1.0");
    assert.equal(report.daemon_health.ready, true);
    assert.equal(report.errors.filter((e) => /no socket/.test(e)).length, 0);
  } finally {
    stopFakeDaemon(fake);
  }
});

test("CLI `setup --print` prints the official MCP fragment", async () => {
  const out = await runMain(["setup", "--print"]);
  const parsed = JSON.parse(out);
  assert.deepEqual(parsed.mcp["computer-use"], { type: "local", command: ["computer-use-mcp"], enabled: true });
});

test("CLI `status` prints daemon health", async () => {
  const fake = startFakeDaemon();
  try {
    const out = await runMain(["status", "--socket", fake.socketPath]);
    assert.match(out, /daemon: v0\.1\.0 ready/);
    assert.match(out, /session: s1 state=active/);
  } finally {
    stopFakeDaemon(fake);
  }
});

test("CLI `session cleanup` stops the active session", async () => {
  const fake = startFakeDaemon();
  try {
    const out = await runMain(["session", "cleanup", "--socket", fake.socketPath]);
    assert.match(out, /stopped session s1 \(was active\)/);
  } finally {
    stopFakeDaemon(fake);
  }
});

test("CLI `doctor` exits nonzero on issues", async () => {
  const dir = mkdtempSync(join(tmpdir(), "cu-oc-cli-doctor-"));
  const socketPath = join(dir, "nope.sock");
  try {
    // Empty PATH so no binaries are found; dead socket path.
    const origPath = process.env.PATH;
    const origExit = process.exitCode;
    process.env.PATH = "";
    process.exitCode = undefined;
    try {
      await runMain(["doctor", "--socket", socketPath]);
    } finally {
      process.env.PATH = origPath;
    }
    assert.equal(process.exitCode, 1);
    process.exitCode = origExit;
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("defaultOpenCodeConfigPath points at the OpenCode config directory", () => {
  const p = defaultOpenCodeConfigPath();
  assert.ok(p.startsWith(homedir()));
  assert.ok(p.endsWith(join(".config", "opencode", "opencode.json")));
});
