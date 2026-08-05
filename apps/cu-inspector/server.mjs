#!/usr/bin/env node
// Minimal local dashboard for the computer-use daemon.
//
// A tiny node:http server (no framework) that proxies the daemon through
// @computer-use/sdk and serves a polling dashboard:
//   GET  /                  → the dashboard HTML
//   GET  /api/health        → daemon health
//   GET  /api/session       → active session (or {error: ...})
//   GET  /api/frame         → fresh observe (metadata only)
//   GET  /api/frame-image   → ?path=<image_path> streams the stored frame
//   GET  /api/traces        → trace list
//   GET  /api/pointer       → current pointer location
//   POST /api/act           → body: {frame_id, actions} executes one batch
//
// Env: COMPUTER_USE_SOCKET (default ~/.computer-use/runtime.sock),
//      CU_INSPECTOR_PORT (default 8420), CU_INSPECTOR_HOST (default 127.0.0.1).
// The server binds to localhost only.

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

import { connect, ComputerUseError } from "@computer-use/sdk";

const HOST = process.env.CU_INSPECTOR_HOST ?? "127.0.0.1";
const PORT = Number(process.env.CU_INSPECTOR_PORT ?? 8420);
const PUBLIC_DIR = join(dirname(fileURLToPath(import.meta.url)), "public");

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".svg": "image/svg+xml",
};

const SOCKET_PATH = process.env.COMPUTER_USE_SOCKET;

// Lazy, resilient connection: the daemon may be down when the inspector
// starts (or may restart later). Each request resolves a client first, so a
// daemon outage surfaces as per-request 502s instead of killing the dashboard.
let client = null;
async function getClient() {
  if (!client) {
    client = await connect({ socketPath: SOCKET_PATH });
    console.error(`cu-inspector connected to ${client.socketPath}`);
  }
  return client;
}
async function withClient(fn) {
  try {
    return await fn(await getClient());
  } catch (err) {
    // A dead/restarted daemon invalidates the socket handle; drop it so the
    // next request reconnects instead of failing forever on a stale handle.
    if (err && (err.code === "DAEMON_UNAVAILABLE" || /closed|not connected/.test(String(err?.message ?? err)))) {
      client = null;
    }
    throw err;
  }
}

async function sessionStatus() {
  try {
    return await withClient((c) => c.session("status", {}));
  } catch {
    return null;
  }
}

async function sendJson(res, status, body) {
  res.writeHead(status, { "content-type": "application/json; charset=utf-8" });
  res.end(JSON.stringify(body, null, 2));
}

async function serveStatic(res, pathname) {
  const safe = pathname === "/" ? "/index.html" : pathname;
  try {
    const body = await readFile(join(PUBLIC_DIR, safe));
    res.writeHead(200, { "content-type": MIME[extname(safe)] ?? "application/octet-stream" });
    res.end(body);
  } catch {
    sendJson(res, 404, { error: "not found" });
  }
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url ?? "/", `http://${HOST}:${PORT}`);
  try {
    if (url.pathname.startsWith("/api/")) {
      switch (url.pathname) {
        case "/api/health": {
          const health = await withClient((c) => c.health());
          return sendJson(res, 200, health);
        }
        case "/api/session": {
          const s = await sessionStatus();
          return sendJson(res, 200, s ?? { error: "NO_ACTIVE_SESSION" });
        }
        case "/api/frame": {
          const s = await sessionStatus();
          if (!s) return sendJson(res, 200, { error: "NO_ACTIVE_SESSION" });
          const frame = await withClient((c) => c.observe({ session_id: s.session_id }));
          return sendJson(res, 200, {
            frame_id: frame.frame_id,
            width: frame.width,
            height: frame.height,
            display_id: frame.display_id,
            scale_factor: frame.scale_factor,
            active_application: frame.active_application ?? null,
            active_window: frame.active_window ?? null,
            image_path: frame.image_path,
            captured_at: frame.captured_at,
          });
        }
        case "/api/frame-image": {
          const path = url.searchParams.get("path");
          if (!path) return sendJson(res, 400, { error: "missing path" });
          try {
            const body = await readFile(path);
            res.writeHead(200, { "content-type": "image/jpeg" });
            return res.end(body);
          } catch {
            return sendJson(res, 404, { error: "image not found" });
          }
        }
        case "/api/traces": {
          const { traces } = await withClient((c) => c.traceList());
          return sendJson(res, 200, { traces });
        }
        case "/api/pointer": {
          const p = await withClient((c) => c.pointer());
          return sendJson(res, 200, p);
        }
        case "/api/act": {
          if (req.method !== "POST") return sendJson(res, 405, { error: "POST only" });
          const body = await new Promise((resolve, reject) => {
            let data = "";
            req.on("data", (c) => (data += c));
            req.on("end", () => {
              try {
                resolve(JSON.parse(data));
              } catch {
                reject(new Error("bad json"));
              }
            });
            req.on("error", reject);
          });
          const s = await sessionStatus();
          if (!s) return sendJson(res, 409, { error: "NO_ACTIVE_SESSION" });
          const result = await withClient((c) =>
            c.act({
              session_id: s.session_id,
              frame_id: body.frame_id,
              actions: body.actions,
            }),
          );
          return sendJson(res, 200, result);
        }
        default:
          return sendJson(res, 404, { error: "unknown endpoint" });
      }
    }
    return serveStatic(res, url.pathname);
  } catch (err) {
    if (err instanceof ComputerUseError) {
      return sendJson(res, 502, { error: err.code, message: err.message, data: err.data });
    }
    return sendJson(res, 500, { error: "INTERNAL", message: String(err) });
  }
});

server.listen(PORT, HOST, () => {
  console.error(`cu-inspector listening on http://${HOST}:${PORT}`);
});
