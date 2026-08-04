// Hermetic tests for the OpenCode companion CLI.
//
// OpenCode consumes the runtime through the MCP server (not a plugin), so
// these tests drive the library functions and CLI `main` against a fake
// computer-use daemon on a temp Unix socket: config generation, status
// reporting, session cleanup, and doctor checks.
import { createServer } from "node:net";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import assert from "node:assert/strict";

import { parse as parseJsonc } from "jsonc-parser";

import {
  cleanupSession,
  defaultOpenCodeConfigPath,
  generateOpenCodeConfig,
  listStoredCredentials,
  mergeOpenCodeConfigText,
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
              owner_client_id: "oc-test-conn-1",
              owner_client_name: "opencode-adapter-test",
            },
          });
        } else if (req.method === "runtime.version") {
          respond({
            jsonrpc: "2.0",
            id: req.id,
            result: { name: "fake", version: "0.1.0", protocol_version: 2 },
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

// Point $HOME at a temp dir (os.homedir() reads $HOME on POSIX) so the
// credential store that cleanupSession/listStoredCredentials read is
// hermetic. Restored on restore().
function withTempHome(tag) {
  const dir = mkdtempSync(join(tmpdir(), `cu-oc-home-${tag}-`));
  const origHome = process.env.HOME;
  process.env.HOME = dir;
  return {
    dir,
    writeCredential(sessionId, controlToken) {
      const credDir = join(dir, ".local", "state", "oc-computer-use", "credentials");
      mkdirSync(credDir, { recursive: true });
      writeFileSync(
        join(credDir, `${sessionId}.json`),
        JSON.stringify({
          session_id: sessionId,
          control_token: controlToken,
          client_instance_id: "cu-test",
          created_at: "2026-08-04T00:00:00Z",
        }),
      );
    },
    restore() {
      process.env.HOME = origHome;
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

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

// ---------------------------------------------------------------------------
// JSONC config merging (spec scenarios)
// ---------------------------------------------------------------------------

const CU_ENTRY = { type: "local", command: ["computer-use-mcp"], enabled: true };

test("JSONC merge: plain JSON document (1/10)", () => {
  const { text, changed, hadEntry } = mergeOpenCodeConfigText(
    '{\n  "$schema": "https://opencode.ai/config.json"\n}\n',
  );
  assert.equal(changed, true);
  assert.equal(hadEntry, false);
  const parsed = JSON.parse(text);
  assert.deepEqual(parsed.mcp["computer-use"], CU_ENTRY);
  assert.equal(parsed.$schema, "https://opencode.ai/config.json");
});

test("JSONC merge: line comments survive (2/10)", () => {
  const src = '// my personal opencode config\n{\n  "$schema": "https://opencode.ai/config.json", // schema\n  "theme": "dark"\n}\n';
  const { text, changed } = mergeOpenCodeConfigText(src);
  assert.equal(changed, true);
  assert.match(text, /\/\/ my personal opencode config/);
  assert.match(text, /\/\/ schema/);
  assert.equal(parseJsonc(text).theme, "dark");
});

test("JSONC merge: block comments survive (3/10)", () => {
  const src = '{\n  /* keep this block\n     comment */\n  "agent": { "default": { "model": "gpt-5" } }\n}\n';
  const { text, changed } = mergeOpenCodeConfigText(src);
  assert.equal(changed, true);
  assert.match(text, /\/\* keep this block/);
  assert.equal(JSON.parse(text.replace(/\/\*[\s\S]*?\*\//g, "")).agent.default.model, "gpt-5");
});

test("JSONC merge: trailing commas survive (4/10)", () => {
  const src = '{\n  "mcp": {\n    "git": { "type": "local", "command": ["git-mcp"], },\n  },\n}\n';
  const { text, changed } = mergeOpenCodeConfigText(src);
  assert.equal(changed, true);
  // The trailing commas are still there (the document remains JSONC).
  assert.match(text, /\],/);
  assert.match(text, /\},/);
  assert.match(text, /\},$/m);
  const parsed = JSON.parse(text.replace(/,(\s*[}\]])/g, "$1"));
  assert.deepEqual(parsed.mcp["computer-use"], CU_ENTRY);
  assert.ok(parsed.mcp.git, "other MCP server preserved");
});

test("JSONC merge: existing MCP servers are untouched (5/10)", () => {
  const src = JSON.stringify({
    mcp: { github: { type: "local", command: ["github-mcp"], enabled: false } },
  });
  const { text, changed } = mergeOpenCodeConfigText(src);
  assert.equal(changed, true);
  const parsed = JSON.parse(text);
  assert.deepEqual(parsed.mcp.github, { type: "local", command: ["github-mcp"], enabled: false });
  assert.deepEqual(parsed.mcp["computer-use"], CU_ENTRY);
});

test("JSONC merge: an existing computer-use entry is replaced (6/10)", () => {
  const src = JSON.stringify({
    mcp: { "computer-use": { type: "stdio", command: ["old"], enabled: true } },
  });
  const { text, changed, hadEntry } = mergeOpenCodeConfigText(src);
  assert.equal(changed, true);
  assert.equal(hadEntry, true);
  const parsed = JSON.parse(text);
  assert.deepEqual(parsed.mcp["computer-use"], CU_ENTRY);
});

test("JSONC merge: corrupt config throws and is never rewritten (7/10)", async () => {
  assert.throws(() => mergeOpenCodeConfigText('{"mcp": {'), /cannot parse/);
  assert.throws(() => mergeOpenCodeConfigText("this is not json"), /cannot parse/);
  // writeOpenCodeConfig leaves a corrupt file alone (no backup, no write).
  const dir = mkdtempSync(join(tmpdir(), "cu-oc-corrupt-"));
  try {
    const path = join(dir, "opencode.json");
    writeFileSync(path, '{"mcp": {', "utf8");
    await assert.rejects(() => writeOpenCodeConfig(path), /cannot parse/);
    assert.equal(readFileSync(path, "utf8"), '{"mcp": {');
    const leftovers = readdirSync(dir).filter((f) => f.includes("backup"));
    assert.equal(leftovers.length, 0, "no backup for a corrupt, unmodified file");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("JSONC merge: comments are preserved together with new entries (8/10)", () => {
  const src = '{\n  "//": "computer-use is managed by cu-opencode setup",\n  "mcp": {\n    "//": "servers",\n    "git": { "type": "local", "command": ["git-mcp"] }\n  }\n}\n';
  const { text } = mergeOpenCodeConfigText(src);
  assert.match(text, /computer-use is managed by cu-opencode setup/);
  assert.match(text, /"\/\/": "servers"/);
  const parsed = JSON.parse(text.replace(/,\s*([}\]])/g, "$1"));
  assert.ok(parsed.mcp.git);
  assert.ok(parsed.mcp["computer-use"]);
});

test("JSONC merge: unknown top-level fields are preserved (9/10)", () => {
  const src = JSON.stringify({
    $schema: "https://opencode.ai/config.json",
    theme: "dark",
    provider: { openai: { apiKey: "env:OPENAI_API_KEY" } },
    model: { "gpt-5": { provider: "openai" } },
    plugin: ["my-plugin"],
    permission: { edit: "allow" },
    agent: { builder: { prompt: "be brief" } },
    tool: { disabled: ["shell"] },
    mcp: {},
  });
  const { text, changed } = mergeOpenCodeConfigText(src);
  assert.equal(changed, true);
  const parsed = JSON.parse(text);
  assert.deepEqual(parsed, { ...JSON.parse(src), mcp: { "computer-use": CU_ENTRY } });
  assert.deepEqual(parsed.provider, { openai: { apiKey: "env:OPENAI_API_KEY" } });
  assert.deepEqual(parsed.agent, { builder: { prompt: "be brief" } });
  assert.deepEqual(parsed.tool, { disabled: ["shell"] });
});

test("JSONC merge: idempotent — a second merge is a no-op (10/10)", () => {
  const src = '{\n  // note\n  "mcp": { "git": { "type": "local", "command": ["git-mcp"] } }\n}\n';
  const first = mergeOpenCodeConfigText(src);
  assert.equal(first.changed, true);
  const second = mergeOpenCodeConfigText(first.text);
  assert.equal(second.changed, false, "second merge changes nothing");
  assert.equal(second.text, first.text);
  assert.equal(second.hadEntry, true);
});

test("writeOpenCodeConfig backs up the original before changing it", async () => {
  const dir = mkdtempSync(join(tmpdir(), "cu-oc-config-"));
  try {
    const path = join(dir, "opencode.json");
    // New file: no backup.
    const first = await writeOpenCodeConfig(path);
    assert.equal(first.existed, false);
    assert.equal(first.merged, false);
    assert.equal(first.changed, true);
    assert.equal(first.backup, null, "no backup for a brand-new file");
    assert.equal(readdirSync(dir).filter((f) => f.includes("backup")).length, 0);
    const written = JSON.parse(readFileSync(path, "utf8"));
    assert.deepEqual(written.mcp["computer-use"].command, ["computer-use-mcp"]);

    // Existing file with an unrelated key: merged, backed up first.
    writeFileSync(path, '{\n  // my comment\n  "tools": { "foo": true }\n}\n', "utf8");
    const second = await writeOpenCodeConfig(path);
    assert.equal(second.existed, true);
    assert.equal(second.changed, true);
    assert.equal(second.merged, false);
    assert.ok(second.backup, "backup created before the change");
    assert.ok(second.backup.includes(".backup-"), `backup name has a timestamp: ${second.backup}`);
    assert.equal(readFileSync(second.backup, "utf8"), '{\n  // my comment\n  "tools": { "foo": true }\n}\n');
    const merged = readFileSync(path, "utf8");
    assert.match(merged, /\/\/ my comment/, "comment survived the merge");
    assert.equal(JSON.parse(merged.replace(/\/\/.*$/gm, "")).tools.foo, true);
    assert.ok(merged.includes('"computer-use"'));

    // No-op run: no new backup, content untouched.
    const third = await writeOpenCodeConfig(path);
    assert.equal(third.changed, false);
    assert.equal(third.backup, null, "no backup for an unchanged config");
    assert.equal(merged, readFileSync(path, "utf8"), "file untouched on a no-op");
    assert.equal(readdirSync(dir).filter((f) => f.includes("backup")).length, 1);
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
    // The owner is shown by name; the control token is never displayed.
    assert.match(text, /owner: opencode-adapter-test/);
    assert.doesNotMatch(text, /control_token|token:/);
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

test("listStoredCredentials reads only well-formed cu credential files", () => {
  const home = withTempHome("list");
  try {
    assert.deepEqual(listStoredCredentials(), []);
    home.writeCredential("s1", "tok-1");
    const credDir = join(home.dir, ".local", "state", "oc-computer-use", "credentials");
    // Corrupt / incomplete files are skipped.
    writeFileSync(join(credDir, "junk.json"), "{not json", "utf8");
    writeFileSync(join(credDir, "incomplete.json"), JSON.stringify({ session_id: "s2" }), "utf8");
    assert.deepEqual(listStoredCredentials(), [{ sessionId: "s1", controlToken: "tok-1" }]);
  } finally {
    home.restore();
  }
});

test("cleanupSession stops exactly the sessions this machine holds credentials for", async () => {
  const fake = startFakeDaemon();
  const home = withTempHome("cred-stop");
  try {
    home.writeCredential("s1", "tok-1");
    home.writeCredential("s2", "tok-2");
    const client = await connect(fake.socketPath);
    try {
      const result = await cleanupSession(client);
      assert.equal(result.stopped, true);
      assert.match(result.message, /stopped sessions s1, s2/);
    } finally {
      client.close();
    }
    const stops = fake.requests.filter((r) => r.method === "computer.session" && r.params?.action === "stop");
    assert.equal(stops.length, 2);
    assert.deepEqual(
      stops.map((s) => s.params.session_id).sort(),
      ["s1", "s2"],
    );
    // Every stop carries the control token — without it the daemon refuses.
    assert.deepEqual(
      stops.map((s) => s.params.control_token).sort(),
      ["tok-1", "tok-2"],
    );
  } finally {
    home.restore();
    stopFakeDaemon(fake);
  }
});

test("cleanupSession without credentials reports an owner-owned session and never stops it", async () => {
  const fake = startFakeDaemon();
  const home = withTempHome("no-creds");
  try {
    const client = await connect(fake.socketPath);
    try {
      const result = await cleanupSession(client);
      assert.equal(result.stopped, false);
      assert.match(result.message, /owned by opencode-adapter-test/);
      assert.match(result.message, /knowing its id does not grant control/);
    } finally {
      client.close();
    }
    // No stop attempt was made: the session belongs to another client.
    assert.equal(fake.requests.filter((r) => r.params?.action === "stop").length, 0);
  } finally {
    home.restore();
    stopFakeDaemon(fake);
  }
});

test("cleanupSession notes a credential whose token the daemon no longer accepts", async () => {
  const fake = startFakeDaemon({
    sessionHandler: (req) => {
      if (req.params?.action === "stop") {
        return {
          jsonrpc: "2.0",
          id: req.id,
          error: {
            code: -32019,
            message: "control token required",
            data: { code: "CONTROL_TOKEN_REQUIRED" },
          },
        };
      }
      return {
        jsonrpc: "2.0",
        id: req.id,
        result: {
          session_id: "s1",
          state: "active",
          paused: false,
          user_takeover: false,
          lock_held: true,
          display_id: "1",
          created_at: "2026-08-04T00:00:00Z",
          started_by: "cu-test",
          owner_client_id: "oc-test-conn-1",
          owner_client_name: "cu-test",
        },
      };
    },
  });
  const home = withTempHome("stale");
  try {
    home.writeCredential("s1", "old-token");
    const client = await connect(fake.socketPath);
    try {
      const result = await cleanupSession(client);
      assert.equal(result.stopped, false);
      assert.match(result.message, /token no longer valid/);
    } finally {
      client.close();
    }
  } finally {
    home.restore();
    stopFakeDaemon(fake);
  }
});

test("cleanupSession is a no-op when no session exists and no credentials are held", async () => {
  const fake = startFakeDaemon({
    sessionHandler: (req) => ({
      jsonrpc: "2.0",
      id: req.id,
      error: { code: -32000, message: "no active session", data: { code: "SESSION_NOT_FOUND" } },
    }),
  });
  const home = withTempHome("none");
  try {
    const client = await connect(fake.socketPath);
    try {
      const result = await cleanupSession(client);
      assert.equal(result.stopped, false);
      assert.match(result.message, /no active session/);
    } finally {
      client.close();
    }
    assert.equal(fake.requests.filter((r) => r.params?.action === "stop").length, 0);
  } finally {
    home.restore();
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

test("CLI `session cleanup` stops the owned session and reports an unowned one", async () => {
  // Phase 1: this machine holds a credential → the owned session is stopped
  // with its control token.
  const homeOwned = withTempHome("cli-cleanup-owned");
  try {
    homeOwned.writeCredential("s1", "tok-1");
    const fake = startFakeDaemon();
    try {
      const out = await runMain(["session", "cleanup", "--socket", fake.socketPath]);
      assert.match(out, /stopped session s1/);
      const stops = fake.requests.filter((r) => r.method === "computer.session" && r.params?.action === "stop");
      assert.equal(stops.length, 1);
      assert.equal(stops[0].params.control_token, "tok-1");
    } finally {
      stopFakeDaemon(fake);
    }
  } finally {
    homeOwned.restore();
  }
  // Phase 2: no credential held → the foreign session is reported, never stopped.
  const homeForeign = withTempHome("cli-cleanup-foreign");
  try {
    const fake2 = startFakeDaemon();
    try {
      const out = await runMain(["session", "cleanup", "--socket", fake2.socketPath]);
      assert.match(out, /does not grant control/);
      assert.equal(fake2.requests.filter((r) => r.params?.action === "stop").length, 0);
    } finally {
      stopFakeDaemon(fake2);
    }
  } finally {
    homeForeign.restore();
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
