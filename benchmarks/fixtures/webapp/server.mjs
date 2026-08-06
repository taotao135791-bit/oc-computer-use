#!/usr/bin/env node
// Local fixture web app for Safari tasks. Real HTTP on 127.0.0.1, so page
// interactions are genuine network side effects that an evaluator can check
// independently of the model:
//
//   GET  /page/<name>            serve a fixture page (public/<name>.html)
//   POST /state {task, value}    page JavaScript reports a UI interaction
//   GET  /check?task=<key>       evaluator endpoint: {"satisfied": bool,
//                                "detail": ...} — satisfied when a POST for
//                                that task key arrived with a truthy value
//   GET  /reset                  clear all state (runner calls this per task)
//   GET  /answer?value=<v>       a page source that echoes <v> (task reads it)
//
// State lives in memory only — nothing is written to disk. Port: 8931 by
// default (override with PORT).
import { createServer } from "node:http";
import { readFileSync, existsSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const PUBLIC_DIR = resolve(dirname(fileURLToPath(import.meta.url)), "public");
const PORT = Number(process.env.PORT || 8931);

const state = new Map(); // task key -> value

function pageHtml(name) {
  const path = join(PUBLIC_DIR, `${name}.html`);
  if (!existsSync(path)) return null;
  return readFileSync(path, "utf8");
}

function send(res, code, body) {
  const b = typeof body === "string" ? body : JSON.stringify(body);
  res.writeHead(code, {
    "Content-Type": typeof body === "string" ? "text/html; charset=utf-8" : "application/json",
    "Cache-Control": "no-store",
  });
  res.end(b);
}

const server = createServer((req, res) => {
  const url = new URL(req.url, `http://127.0.0.1:${PORT}`);
  const segs = url.pathname.split("/").filter(Boolean);

  if (req.method === "GET" && segs[0] === "page" && segs[1]) {
    const html = pageHtml(segs[1]);
    if (!html) return send(res, 404, "<h1>not found</h1>");
    return send(res, 200, html);
  }
  if (req.method === "POST" && segs[0] === "state") {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      try {
        const p = JSON.parse(body);
        state.set(p.task, p.value);
        send(res, 200, { ok: true });
      } catch {
        send(res, 400, { ok: false, detail: "bad state payload" });
      }
    });
    return;
  }
  if (req.method === "GET" && segs[0] === "check") {
    const task = url.searchParams.get("task");
    const satisfied = state.has(task) && state.get(task) !== "" && state.get(task) !== false;
    return send(res, 200, { satisfied, detail: satisfied ? `state recorded: ${JSON.stringify(state.get(task))}` : `no state for '${task}'` });
  }
  if (req.method === "GET" && segs[0] === "reset") {
    state.clear();
    return send(res, 200, { ok: true });
  }
  if (req.method === "GET" && segs[0] === "answer") {
    // A page that displays a server-chosen value; the model must read it,
    // retype it into the page's input, and submit.
    const value = url.searchParams.get("value") || "default-answer";
    return send(res, 200, pageAnswerHtml(value));
  }
  send(res, 404, "<h1>not found</h1>");
});

function pageAnswerHtml(value) {
  return `<!doctype html><html><head><meta charset="utf-8"><title>Answer</title></head>
<body>
<p id="answer">${value}</p>
<input id="input" placeholder="type the answer here">
<button id="submit">Submit</button>
<script>
  document.getElementById("submit").addEventListener("click", () => {
    fetch("/state", { method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ task: "answer", value: document.getElementById("input").value }) });
  });
</script>
</body></html>`;
}

server.listen(PORT, "127.0.0.1", () => {
  console.log(`fixture web app on http://127.0.0.1:${PORT}`);
});
