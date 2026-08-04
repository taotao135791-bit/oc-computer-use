#!/usr/bin/env node
// Real-daemon acceptance for the Pi extension: loads the *built* extension
// (dist/index.js, unmodified) into a minimal Pi-host shim and exercises every
// tool/command against the REAL computer-use daemon and the REAL screen.
//
// The only simulated piece is the ExtensionAPI host surface (registerTool /
// registerCommand / on(session_shutdown) / ctx.ui.notify) — the Pi desktop app
// itself is not available in this environment. Tool/command logic, session
// ownership, screenshot storage and the daemon are all real.
//
// Usage:
//   node scripts/pi-host-acceptance.mjs
// Prereqs: daemon running (`cu daemon start`), no active session (fresh
// scenario), TextEdit open on the desktop (safe click target).
import { spawnSync } from "node:child_process";
import { readFileSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import assert from "node:assert/strict";

const ROOT = join(import.meta.dirname ?? new URL(".", import.meta.url).pathname, "..");
const DIST = join(ROOT, "packages", "pi-extension", "dist", "index.js");
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
const cuSessionStatus = () => {
  const r = cu(["session", "status", "--json"]);
  // No active session: the CLI prints the SESSION_NOT_FOUND error and exits 1
  // with empty stdout → treat as "no session".
  if (!r.ok || !r.json || !r.json.session_id) return null;
  return r.json;
};

// --- Minimal Pi host --------------------------------------------------------

function makeHost() {
  const notifications = [];
  const tools = new Map();
  const commands = new Map();
  const shutdownHandlers = [];
  return {
    notifications,
    tools,
    commands,
    shutdownHandlers,
    api: {
      registerTool(t) {
        tools.set(t.name, t);
      },
      registerCommand(name, c) {
        commands.set(name, c);
      },
      on(event, cb) {
        if (event === "session_shutdown") shutdownHandlers.push(cb);
      },
    },
    ctx: {
      ui: {
        notify: (text, level) => notifications.push({ text, level }),
      },
    },
  };
}

async function loadExtension() {
  const host = makeHost();
  const mod = await import(`${pathToFileURL(DIST).href}?t=${Date.now()}`);
  mod.default(host.api);
  return host;
}

const callTool = async (tool, params, signal) => tool.execute("acceptance-1", params, signal);
const callCommand = async (host, name, args = "") => {
  const c = host.commands.get(name);
  await c.handler(args, host.ctx);
};
const notifyText = (host) => host.notifications.map((n) => n.text).join("\n");

// --- Steps ------------------------------------------------------------------

section("Pi acceptance — 1. daemon up, 2. no active session");
const s0 = cuSessionStatus();
check("daemon running", cu(["daemon", "status"]).ok);
check("no active session at start", s0 === null || (s0 && s0.state === "stopped"), s0 ? `session ${s0.session_id} state=${s0.state}` : "SESSION_NOT_FOUND");

section("Pi acceptance — 3..5. host loads the extension, computer-status");
const host = await loadExtension();
check("extension loads and registers 4 tools", host.tools.size === 4, [...host.tools.keys()].join(", "));
check("extension registers 8 commands", host.commands.size === 8, [...host.commands.keys()].join(", "));
await callCommand(host, "computer-status");
const statusNotify = notifyText(host);
check(
  "/computer-status reports real daemon + no session",
  /daemon: v0\.1\.0 ready/.test(statusNotify) && statusNotify.includes("session: none"),
  statusNotify.replace(/\n/g, " | "),
);

section("Pi acceptance — 6..8. first observe auto-creates the session, real screenshot");
const obs1 = await callTool(host.tools.get("computer_observe"), { include_image: true });
const obsImage = obs1.content.find((b) => b.type === "image");
check("observe returns an image content block", !!obsImage && /^[A-Za-z0-9+/=]{100,}$/.test(obsImage.data));
const obsBytes = Buffer.from(obsImage.data, "base64");
const isJpeg = obsBytes[0] === 0xff && obsBytes[1] === 0xd8 && obsBytes[2] === 0xff;
const isPng = obsBytes[0] === 0x89 && obsBytes[1] === 0x50;
check("screenshot is a real image (JPEG/PNG magic bytes)", isJpeg || isPng, obsImage.mimeType);
const obsText = obs1.content.find((b) => b.type === "text").text;
check("observe text carries real frame_id + size", /frame_id: frame_\d+/.test(obsText) && /size: \d+x\d+/.test(obsText));
const sidPi = obs1.details.session_id;
check("session auto-created on first observe", !!sidPi && /^s_/.test(sidPi), sidPi);
const stPi = cuSessionStatus();
check(
  "auto-created session is owned by pi-extension",
  stPi?.owner_client_id === "pi-extension" && stPi?.owner_client_name === "Pi",
  `owner=${stPi?.owner_client_id}/${stPi?.owner_client_name}/${stPi?.owner_instance_id}`,
);
const obs2 = await callTool(host.tools.get("computer_observe"), {});
check(
  "second observe reuses the session (no second start)",
  obs2.details.session_id === sidPi,
  `session ${obs2.details.session_id}`,
);

section("Pi acceptance — 9. act on a safe target");
// The frame for acts/inspect is the most recent observe (obs2); acting on the
// older obs1 frame would be a legitimate STALE_FRAME under the strict policy.
const frameId = obs2.details.frame_id;
const act1 = await callTool(host.tools.get("computer_act"), {
  frame_id: frameId,
  actions: [{ type: "move", x: 500, y: 400 }, { type: "wait", duration_ms: 100 }],
  return_screenshot: true,
});
check(
  "act per-action success reports",
  act1.details.executed && act1.details.action_results.every((r) => r.status === "success"),
  JSON.stringify(act1.details.action_results),
);
check("act returns a post-batch screenshot image", act1.content.some((b) => b.type === "image"));

section("Pi acceptance — 10..11. inspect a region, image visible to the model");
const inspect = await callTool(host.tools.get("computer_inspect"), {
  frame_id: frameId,
  region: { x: 0, y: 0, width: 200, height: 200, coordinate_space: "normalized_1000" },
  scale: 2,
});
const inspImage = inspect.content.find((b) => b.type === "image");
check("inspect returns a cropped image block", !!inspImage && inspImage.data.length > 100);
const inspText = inspect.content.find((b) => b.type === "text").text;
check(
  "inspect text carries mapping origins",
  /global_origin: \d+,\d+/.test(inspText) && /normalized_1000_origin: \d+,\d+/.test(inspText),
  inspText.replace(/\n/g, " | "),
);

section("Pi acceptance — 12..17. takeover / resume / release via commands");
await callCommand(host, "computer-takeover");
let st = cuSessionStatus();
check("takeover → state user_takeover", st?.user_takeover === true);
let actErr = null;
try {
  await callTool(host.tools.get("computer_act"), {
    frame_id: frameId,
    actions: [{ type: "move", x: 500, y: 400 }],
  });
} catch (e) {
  actErr = e;
}
check("act rejected under takeover (USER_TAKEOVER)", actErr?.code === "USER_TAKEOVER", actErr ? `${actErr.code}: ${actErr.message}` : "no error");
await callCommand(host, "computer-resume");
st = cuSessionStatus();
check("resume cannot bypass takeover (USER_TAKEOVER_ACTIVE)", st?.user_takeover === true, `state=${st?.state}`);
await callCommand(host, "computer-release");
st = cuSessionStatus();
check("release → state active again", st?.user_takeover === false && st?.state === "active");
// The frame advanced while the session was under takeover; re-observe so the
// post-release act runs against a current frame (a stale one is correctly
// rejected under the strict policy).
const obsPost = await callTool(host.tools.get("computer_observe"), {});
const act2 = await callTool(host.tools.get("computer_act"), {
  frame_id: obsPost.details.frame_id,
  actions: [{ type: "wait", duration_ms: 50 }],
});
check("act succeeds after release", act2.details.executed, JSON.stringify(act2.details.action_results));

section("Pi acceptance — screenshot saving (tmpdir, 0600, MIME extension)");
await callCommand(host, "computer-observe");
const saveNotify = notifyText(host).split("\n").filter((l) => l.startsWith("screenshot saved:"));
check("/computer-observe reports a saved screenshot", saveNotify.length === 1, saveNotify[0] ?? "none");
const savedPath = saveNotify[0]?.match(/screenshot saved: (\S+)/)?.[1];
let savedStat = null;
let magic = "";
if (savedPath) {
  savedStat = statSync(savedPath);
  magic = readFileSync(savedPath).subarray(0, 3).toString("hex");
}
check(
  "screenshot is in the system temp dir (not the repo/cwd)",
  savedPath && savedPath.startsWith(tmpdir()) && !savedPath.includes(ROOT),
  savedPath ?? "no path",
);
check("screenshot is a real JPEG (ffd8ff magic)", magic === "ffd8ff", magic);
check("screenshot perms are 0600", savedStat && (savedStat.mode & 0o777) === 0o600, `mode=${(savedStat?.mode ?? 0).toString(8)}`);

section("Pi acceptance — 18..19. session_shutdown stops only the session Pi created");
const sidBefore = cuSessionStatus()?.session_id;
for (const cb of host.shutdownHandlers) await cb();
const stAfter = cuSessionStatus();
check(
  "Pi-created session stopped on shutdown",
  stAfter === null || stAfter.state === "stopped",
  stAfter ? `${stAfter.session_id} state=${stAfter.state}` : "SESSION_NOT_FOUND",
);
check("screenshot cleaned up on shutdown", !savedPath || statSync(savedPath, { throwIfNoEntry: false }) === undefined);
if (savedPath) {
  try {
    statSync(savedPath);
    check("screenshot removed", false);
  } catch {
    check("screenshot removed", true);
  }
}

section("Pi acceptance — 20. foreign session: reject policy then attach policy");
// Scenario A: another client (CLI) owns the session; default policy rejects.
const started = cu(["session", "start", "--json"]);
const foreignSid = started.json?.session_id;
check("CLI started a foreign session", !!foreignSid, foreignSid);
let locked = null;
try {
  await callTool(host.tools.get("computer_observe"), {});
} catch (e) {
  locked = e;
}
check(
  "observe on foreign session → CONTROL_LOCKED (reject policy)",
  locked?.code === "CONTROL_LOCKED" && locked?.message === "Another client owns the active computer-use session.",
  locked ? `${locked.code}: ${locked.message}` : "no error",
);

// Scenario B: attach policy — observe works, shutdown does not stop it.
const host2 = await loadExtension();
process.env.COMPUTER_USE_EXISTING_SESSION_POLICY = "attach";
try {
  const obsA = await callTool(host2.tools.get("computer_observe"), {});
  check(
    "attach policy: observe works on the foreign session",
    obsA.details.session_id === foreignSid,
    `session ${obsA.details.session_id}`,
  );
  for (const cb of host2.shutdownHandlers) await cb();
  const stF = cuSessionStatus();
  check(
    "attach policy: shutdown does NOT stop the foreign session",
    stF?.session_id === foreignSid && stF?.state === "active",
    `session ${stF?.session_id} state=${stF?.state}`,
  );
} finally {
  delete process.env.COMPUTER_USE_EXISTING_SESSION_POLICY;
}

// Cleanup: stop the foreign session (we own it via CLI).
cu(["session", "stop"]);

console.log(`\n=== ${results.filter((r) => r.ok).length}/${results.length} PASS ===`);
if (results.some((r) => !r.ok)) {
  process.exit(1);
}
