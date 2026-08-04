#!/usr/bin/env node
// Real-daemon acceptance for the OpenCode path: spawns the *same*
// `computer-use-mcp` binary OpenCode launches (from the config written by
// `cu-opencode setup`), speaks newline-delimited MCP over stdio exactly as
// OpenCode does, and drives the computer-use tools against the REAL daemon
// and REAL screen.
//
// The only thing not exercised here is a model choosing to call the tools —
// that needs a working OpenCode model provider (the zhipuai plan on this
// machine is expired).
//
// Usage: node scripts/opencode-mcp-acceptance.mjs
// Prereqs: daemon running, no active session, TextEdit open with a document.
import { spawnSync } from "node:child_process";
import { spawn } from "node:child_process";
import { join } from "node:path";
import assert from "node:assert/strict";

const ROOT = join(import.meta.dirname ?? new URL(".", import.meta.url).pathname, "..");
const CU = join(ROOT, "target", "release", "cu");

const results = [];
function check(step, ok, detail) {
  results.push({ step, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${step}${detail ? ` — ${detail}` : ""}`);
}
function section(title) {
  console.log(`\n=== ${title} ===`);
}

function cu(args) {
  const r = spawnSync(CU, args, { encoding: "utf8" });
  try {
    return { ok: r.status === 0, json: JSON.parse(r.stdout || "{}"), status: r.status, raw: r.stdout + r.stderr };
  } catch {
    return { ok: r.status === 0, json: null, status: r.status, raw: r.stdout + r.stderr };
  }
}

// --- Minimal MCP-over-stdio client (newline-delimited, like OpenCode) -------
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
        if (msg.error) {
          const err = new Error(msg.error.message);
          err.code = msg.error.data?.code ?? msg.error.code;
          err.data = msg.error.data;
          reject(err);
        } else resolve(msg.result);
      }
    }
  }
  async initialize() {
    const result = await this.request("initialize", {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "opencode-acceptance", version: "0.1.0" },
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

section("OpenCode acceptance — 1..5. daemon, no session, MCP loads, tools exist");
check("daemon running", cu(["daemon", "status"]).ok);
const pre = cu(["session", "status", "--json"]);
check("no active session before first observe", !pre.ok || !pre.json?.session_id, pre.json ? pre.json.session_id : "SESSION_NOT_FOUND");

const proc = spawn("computer-use-mcp", [], {
  env: { ...process.env },
  stdio: ["pipe", "pipe", "pipe"],
});
const client = new McpClient(proc);
let stderr = "";
proc.stderr.on("data", (c) => (stderr += c));
const init = await client.initialize();
check("MCP server initializes (as OpenCode sees it)", init.serverInfo.name === "computer-use", JSON.stringify(init.serverInfo));
const { tools } = await client.listTools();
const names = tools.map((t) => t.name);
const expected = ["computer_act", "computer_cancel", "computer_inspect", "computer_observe", "computer_session", "trace_get", "trace_list"];
check("tools/list returns the computer-use tools", expected.every((n) => names.includes(n)), names.join(", "));

section("OpenCode acceptance — 6..8. first observe auto-creates the session, real image content");
const obs = await client.callTool("computer_observe", { include_image: true });
check("first computer_observe succeeds", obs.isError === undefined);
const image = obs.content.find((b) => b.type === "image");
check("model receives a real MCP image content block", !!image && /^[A-Za-z0-9+/=]{100,}$/.test(image.data), image?.mimeType);
const bytes = Buffer.from(image.data, "base64");
const isJpeg = bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff;
check("image is a real screenshot (JPEG magic)", isJpeg);
const obsText = obs.content.find((b) => b.type === "text").text;
const frameId = obsText.match(/frame_id: (frame_\d+)/)?.[1];
check("observe text carries frame_id", !!frameId, frameId);
const st = cu(["session", "status", "--json"]).json;
check(
  "session auto-created, owned by mcp-server",
  st?.owner_client_id === "mcp-server" && st?.owner_client_name === "Computer Use MCP",
  `owner=${st?.owner_client_id}/${st?.owner_client_name}/${st?.owner_instance_id} session=${st?.session_id}`,
);
const obs2 = await client.callTool("computer_observe", { include_image: false });
check("second observe reuses the session (no second start)", obs2.isError === undefined);

// MCP tool errors arrive as a successful RPC with `isError: true` content
// (the server never throws) — read the error text out of the content.
const errText = (res) => (res.isError ? res.content[0].text : null);

section("OpenCode acceptance — 9. safe click on the frame");
// obs2 was the last observe → its frame is current. (A successful act with
// return_screenshot captures a NEW frame, so the next act must use the newest.)
const frameB = obs2.content[0].text.match(/frame_id: (frame_\d+)/)?.[1];
const act1 = await client.callTool("computer_act", {
  session_id: st.session_id,
  frame_id: frameB,
  actions: [{ type: "move", x: 500, y: 400 }, { type: "wait", duration_ms: 100 }],
});
check("act succeeds with per-action reports", act1.isError === undefined && /action\[1\]: success/.test(act1.content[0].text), act1.content[0].text.replace(/\n/g, " | "));

section("OpenCode acceptance — 10..11. switch window → STALE_FRAME on old frame");
spawnSync("osascript", ["-e", "tell application \"Google Chrome\" to activate"], { timeout: 5000 });
await new Promise((r) => setTimeout(r, 1500));
const staleRes = await client.callTool("computer_act", {
  session_id: st.session_id,
  frame_id: frameB, // the frame observed before the window switch
  actions: [{ type: "move", x: 500, y: 500 }],
});
const staleErr = errText(staleRes);
check(
  "act on the pre-switch frame is rejected with STALE_FRAME",
  staleErr !== null && staleErr.includes("STALE_FRAME"),
  staleErr ?? "no error",
);

section("OpenCode acceptance — 12. re-observe then continue");
const obs3 = await client.callTool("computer_observe", { include_image: false });
const frame3 = obs3.content[0].text.match(/frame_id: (frame_\d+)/)?.[1];
const act2 = await client.callTool("computer_act", {
  session_id: st.session_id,
  frame_id: frame3,
  actions: [{ type: "wait", duration_ms: 50 }],
});
check("act succeeds after re-observe", act2.isError === undefined && /action\[0\]: success/.test(act2.content[0].text));

section("OpenCode acceptance — 13. cancel a long wait");
// Re-observe so the wait runs on a current frame (the previous act captured
// a new one via return_screenshot).
const obs4 = await client.callTool("computer_observe", { include_image: false });
const frame4 = obs4.content[0].text.match(/frame_id: (frame_\d+)/)?.[1];
const act3 = client.callTool("computer_act", {
  session_id: st.session_id,
  frame_id: frame4,
  actions: [{ type: "wait", duration_ms: 30000 }],
});
await new Promise((r) => setTimeout(r, 700));
const canc = await client.callTool("computer_cancel", { session_id: st.session_id });
check("computer_cancel accepted", canc.isError === undefined && /cancelled: true/.test(canc.content[0].text), canc.content[0].text);
const t0 = Date.now();
const act3res = await act3;
check(
  "the 30s wait stops fast with cancelled report",
  act3res.isError === undefined && /action\[0\]: cancelled/.test(act3res.content[0].text),
  `${act3res.content[0].text.replace(/\n/g, " | ")} (elapsed ${Date.now() - t0}ms)`,
);

section("OpenCode acceptance — 14. stop the session");
const stop = await client.callTool("computer_session", { action: "stop", session_id: st.session_id });
check("session stop succeeds", stop.isError === undefined && /state: stopped/.test(stop.content[0].text), stop.content[0].text.split("\n")[1]);
const after = cu(["session", "status", "--json"]);
check("no active session after stop", !after.ok || !after.json?.session_id);

proc.kill();
console.log(`\n=== ${results.filter((r) => r.ok).length}/${results.length} PASS ===`);
if (results.some((r) => !r.ok)) process.exit(1);
