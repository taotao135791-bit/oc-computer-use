#!/usr/bin/env node
// Ownership scenario A (spec §十一): OpenCode/MCP creates the session,
// the Pi extension must not take it over (default `reject` policy) and must
// not stop it on shutdown — the session survives Pi exit.
//
// Usage: node scripts/ownership-scenario-a.mjs
// Prereqs: daemon running, no active session.
import { spawnSync, spawn } from "node:child_process";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const CU = join(ROOT, "target", "release", "cu");

const results = [];
function check(step, ok, detail) {
  results.push({ step, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${step}${detail ? ` — ${detail}` : ""}`);
}
function cu(args) {
  const r = spawnSync(CU, args, { encoding: "utf8" });
  try {
    return { ok: r.status === 0, json: JSON.parse(r.stdout || "{}"), status: r.status, raw: r.stdout + r.stderr };
  } catch {
    return { ok: r.status === 0, json: null, status: r.status, raw: r.stdout + r.stderr };
  }
}

// --- minimal MCP client with exit + timeout protection ---------------------
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
    proc.on("exit", (code, signal) => {
      const why = `MCP process exited (code=${code} signal=${signal})`;
      for (const { reject } of this.pending.values()) reject(new Error(why));
      this.pending.clear();
    });
    proc.stderr.on("data", (c) => (this.stderr = (this.stderr ?? "") + c));
  }
  send(obj) {
    this.proc.stdin.write(`${JSON.stringify(obj)}\n`);
  }
  request(method, params, timeoutMs = 30_000) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const t = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`timeout after ${timeoutMs}ms waiting for ${method}`));
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (v) => { clearTimeout(t); resolve(v); },
        reject: (e) => { clearTimeout(t); reject(e); },
      });
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
        if (msg.error) {
          const err = new Error(msg.error.message);
          err.code = msg.error.data?.code ?? msg.error.code;
          reject(err);
        } else resolve(msg.result);
      }
    }
  }
  async initialize() {
    const result = await this.request("initialize", {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "ownership-scenario-a", version: "0.1.0" },
    });
    this.send({ jsonrpc: "2.0", method: "notifications/initialized" });
    return result;
  }
  callTool(name, args) {
    return this.request("tools/call", { name, arguments: args });
  }
}

// --- 1. OpenCode path creates the session -----------------------------------
const pre = cu(["session", "status", "--json"]);
check("no active session before the scenario", !pre.ok || !pre.json?.session_id);
console.log("pre =", JSON.stringify(pre.json));

const proc = spawn("computer-use-mcp", [], { stdio: ["pipe", "pipe", "pipe"] });
const client = new McpClient(proc);
await client.initialize();
const obs = await client.callTool("computer_observe", { include_image: true });
check("OpenCode (MCP) first observe creates a session", !obs.isError, client.stderr?.slice(-200));
const obsText = obs.content.find((b) => b.type === "text").text;
const sid = obsText.match(/session_id: (s_\w+)/)?.[1];
const st = cu(["session", "status", "--json"]).json;
check(
  "session owned by mcp-server (OpenCode's MCP)",
  st?.owner_client_id === "mcp-server" && st?.session_id === sid,
  `owner=${st?.owner_client_id}/${st?.owner_client_name} session=${st?.session_id}`,
);

// --- 2. Pi extension (real code, host shim) detects the session --------------
const DIST = join(ROOT, "packages", "pi-extension", "dist", "index.js");
const host = {
  tools: new Map(),
  commands: new Map(),
  shutdown: [],
  api: {
    registerTool: (t) => host.tools.set(t.name, t),
    registerCommand: (n, c) => host.commands.set(n, c),
    on: (e, cb) => {
      if (e === "session_shutdown") host.shutdown.push(cb);
    },
  },
  ctx: { ui: { notify: () => {} } },
};
const mod = await import(`${pathToFileURL(DIST).href}?t=${Date.now()}`);
mod.default(host.api);

let locked = null;
try {
  await host.tools.get("computer_observe").execute("1", { include_image: true });
} catch (e) {
  locked = e;
}
check(
  "Pi observe on the MCP-owned session is rejected with CONTROL_LOCKED (default reject policy)",
  locked?.code === "CONTROL_LOCKED",
  locked ? `${locked.code}: ${locked.message}` : "no error (unexpected)",
);

// --- 3. Pi exits: shutdown callbacks fire, but must NOT stop the session -----
for (const cb of host.shutdown) await cb();
const after = cu(["session", "status", "--json"]).json;
check(
  "after Pi shutdown the MCP-owned session is still active",
  after?.session_id === sid && after?.state === "active",
  `session=${after?.session_id} state=${after?.state}`,
);

// --- cleanup ------------------------------------------------------------------
const stop = await client.callTool("computer_session", { action: "stop", session_id: sid });
check("cleanup: OpenCode stops its own session", !stop.isError, stop.content?.[0]?.text?.split("\n")[1]);
proc.kill();

console.log(`\n=== ${results.filter((r) => r.ok).length}/${results.length} PASS ===`);
if (results.some((r) => !r.ok)) process.exit(1);
