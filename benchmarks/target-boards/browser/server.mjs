// Browser Target Board record server: 本地静态页面 + 点击记录收集 + 统计。
// 用法: node benchmarks/target-boards/browser/server.mjs [port]
import http from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const PORT = Number(process.argv[2] || 8765);
const records = [];

const server = http.createServer(async (req, res) => {
  const url = new URL(req.url, `http://localhost:${PORT}`);

  if (url.pathname === "/" && req.method === "GET") {
    const html = await readFile(path.join(__dirname, "index.html"));
    res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
    res.end(html);
    return;
  }

  if (url.pathname === "/api/record" && req.method === "POST") {
    let body = "";
    for await (const chunk of req) body += chunk;
    try {
      const rec = JSON.parse(body);
      records.push(rec);
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ ok: true, count: records.length }));
    } catch (e) {
      res.writeHead(400);
      res.end(JSON.stringify({ ok: false, error: e.message }));
    }
    return;
  }

  if (url.pathname === "/api/stats" && req.method === "GET") {
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify(computeStats(), null, 2));
    return;
  }

  if (url.pathname === "/api/reset" && req.method === "POST") {
    records.length = 0;
    res.writeHead(200);
    res.end(JSON.stringify({ ok: true }));
    return;
  }

  res.writeHead(404);
  res.end("not found");
});

function computeStats() {
  const total = records.length;
  if (total === 0) return { total: 0 };

  const hits = records.filter((r) => r.hit).length;
  const errors = records.map((r) => r.center_error_px ?? 0).sort((a, b) => a - b);
  const p50 = errors[Math.floor(errors.length * 0.5)] ?? 0;
  const p95 = errors[Math.floor(errors.length * 0.95)] ?? 0;

  const bySize = {};
  for (const r of records) {
    bySize[r.size] = bySize[r.size] || { total: 0, hit: 0 };
    bySize[r.size].total++;
    if (r.hit) bySize[r.size].hit++;
  }
  const byType = {};
  for (const r of records) {
    byType[r.type] = byType[r.type] || { total: 0, hit: 0 };
    byType[r.type].total++;
    if (r.hit) byType[r.type].hit++;
  }

  return {
    total,
    hit_rate: hits / total,
    p50,
    p95,
    by_size: bySize,
    by_type: byType,
  };
}

server.listen(PORT, () => {
  console.log(`Target Board server on http://localhost:${PORT}`);
  console.log(`  GET /          — 页面`);
  console.log(`  GET /api/stats — 统计`);
  console.log(`  POST /api/reset — 清空`);
});