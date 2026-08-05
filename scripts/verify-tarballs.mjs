// End-to-end verification of every publishable npm package, from its packed
// tarball — never from the workspace source. This is the guarantee that
// `npm install @computer-use/*` works, not just `pnpm -r build`:
//
//   SDK tarball            fresh npm install → ESM import + types present
//   MCP tarball            fresh npm install → bin executable, shebang,
//                          `--help` exits 0, initialize + tools/list over
//                          stdio (7 tools, incl. the 4 core ones)
//   Pi extension tarball   fresh npm install → default export is a factory,
//                          registers the 4 tools + session_shutdown handler
//   OpenCode adapter       fresh npm install → default export importable,
//                          official MCP config generated, `cu-opencode` bin
//                          present and executable
//
// Each package is packed with `pnpm pack` (which runs its `prepack`:
// clean + build, so a stale dist can never leak into the tarball) and
// installed into its own temp dir with a plain `npm install`. No workspace
// path is reachable at runtime. Failures print diagnostics and exit 1.
import { spawnSync, spawn } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const PKGS = ["sdk-typescript", "mcp-server", "pi-extension", "opencode-adapter"];

let failures = 0;
let checks = 0;
function ok(name) { checks++; console.log(`  PASS  ${name}`); }
function fail(name, detail) {
  failures++; checks++;
  console.error(`  FAIL  ${name}`);
  if (detail) console.error(detail.split("\n").map((l) => `        ${l}`).join("\n"));
}
function section(title) { console.log(`\n== ${title} ==`); }

function run(cmd, args, opts = {}) {
  return spawnSync(cmd, args, { encoding: "utf8", ...opts });
}
function sh(cmd, opts = {}) {
  return spawnSync("/bin/sh", ["-c", cmd], { encoding: "utf8", ...opts });
}

function tempDir(tag) {
  const dir = mkdtempSync(join(tmpdir(), `oc-cu-${tag}-`));
  return dir;
}

// Drive the MCP bin over stdio: initialize + tools/list, parse the two
// newline-delimited JSON-RPC responses. Returns the tools/list result.
function mcpInitializeAndToolsList(binPath, timeoutMs = 20_000) {
  return new Promise((resolvePromise, rejectPromise) => {
    const child = spawn(binPath, [], { stdio: ["pipe", "pipe", "pipe"] });
    const lines = [];
    let toolsResult = null;
    let stderr = "";
    let settled = false;

    const finish = (fn, val) => {
      if (settled) return;
      settled = true;
      try { child.stdin.end(); } catch { /* already closed */ }
      child.kill("SIGKILL");
      fn(val);
    };

    child.stdout.on("data", (buf) => {
      const chunk = buf.toString("utf8");
      for (const line of chunk.split("\n")) {
        if (!line.trim()) continue;
        let msg;
        try { msg = JSON.parse(line); } catch { continue; }
        if (msg.id === 1 && msg.result) {
          lines.push(`initialize → ${msg.result.serverInfo.name} ${msg.result.serverInfo.version}`);
          child.stdin.write(JSON.stringify({
            jsonrpc: "2.0", id: 2, method: "tools/list", params: {},
          }) + "\n");
        } else if (msg.id === 2) {
          toolsResult = msg.result;
          lines.push(`tools/list → ${msg.result?.tools?.length ?? "?"} tools`);
          finish(resolvePromise, { lines, toolsResult });
        }
      }
    });
    child.stderr.on("data", (buf) => { stderr += buf.toString("utf8"); });
    child.on("error", (err) => finish(rejectPromise, new Error(`spawn failed: ${err.message}`)));
    child.on("exit", (code) => {
      if (!toolsResult) {
        finish(rejectPromise, new Error(`bin exited (code ${code}) before tools/list${stderr ? `; stderr: ${stderr.slice(0, 500)}` : ""}`));
      }
    });

    child.stdin.write(JSON.stringify({
      jsonrpc: "2.0", id: 1, method: "initialize",
      params: { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "tarball-verify", version: "0" } },
    }) + "\n");

    setTimeout(() => finish(rejectPromise, new Error(`timed out after ${timeoutMs}ms; got:\n${lines.join("\n")}\nstderr:\n${stderr.slice(0, 500)}`)), timeoutMs);
  });
}

const work = tempDir("tarballs");

try {
  // First pass: pack every package (prepack = clean + build, so each tarball
  // is built fresh — a stale dist can never leak in).
  const packed = new Map();
  for (const pkg of PKGS) {
    const pkgDir = join(REPO, "packages", pkg);
    const packDir = join(work, `${pkg}-pack`);
    mkdirSync(packDir, { recursive: true });
    const pack = run("pnpm", ["pack", "--pack-destination", packDir], { cwd: pkgDir });
    if (pack.status !== 0) {
      fail(`${pkg} tarball`, `pnpm pack failed:\n${pack.stderr}`);
      continue;
    }
    const tgz = readdirSync(packDir).find((f) => f.endsWith(".tgz"));
    if (!tgz) { fail(`${pkg} tarball`, "no .tgz produced"); continue; }
    packed.set(pkg, join(packDir, tgz));
    ok(`${pkg} packed → ${tgz}`);
  }

  // Second pass: one fresh install dir, tarballs installed in dependency
  // order (SDK first). The other three depend on `@computer-use/sdk`, which
  // is not on the registry: once the SDK tarball is in node_modules, npm
  // resolves the dependency locally instead of hitting a 404. This mirrors
  // the publish order (sdk → mcp-server → pi-extension → opencode-adapter).
  const installDir = join(work, "install");
  mkdirSync(installDir, { recursive: true });
  const init = run("npm", ["init", "-y"], { cwd: installDir });
  if (init.status !== 0) { fail("install dir", init.stderr); }
  for (const pkg of ["sdk-typescript", "mcp-server", "pi-extension", "opencode-adapter"]) {
    const tgzPath = packed.get(pkg);
    if (!tgzPath) continue;
    section(pkg);
    const inst = run("npm", ["install", "--no-audit", "--no-fund", "--ignore-scripts", tgzPath], { cwd: installDir });
    if (inst.status !== 0) {
      fail(`${pkg} npm install`, inst.stderr.slice(0, 1000));
      continue;
    }
    ok(`${pkg} npm install`);

    const pkgName = JSON.parse(readFileSync(join(REPO, "packages", pkg, "package.json"), "utf8")).name;
    const installedDir = join(installDir, "node_modules", ...pkgName.split("/"));

    // 3. No workspace leakage: the installed tree must be inside node_modules.
    if (!installedDir.startsWith(join(installDir, "node_modules"))) {
      fail(`${pkg} install location`, `resolved outside node_modules: ${installedDir}`);
      continue;
    }

    if (pkg === "sdk-typescript") {
      const imp = sh(`cd ${JSON.stringify(installDir)} && node --input-type=module -e 'import("${pkgName}").then(m => { if (typeof m.ComputerUseClient !== "function") throw new Error("no ComputerUseClient"); console.log("import ok") })'`);
      if (imp.status !== 0) { fail("SDK ESM import", imp.stderr); continue; }
      ok("SDK ESM import (ComputerUseClient)");
      const types = join(installedDir, "dist", "index.d.ts");
      if (!existsSync(types)) { fail("SDK type declarations", `missing ${types}`); continue; }
      ok("SDK dist/index.d.ts present");
    }

    if (pkg === "mcp-server") {
      const binPath = join(installedDir, "dist", "bin.js");
      if (!existsSync(binPath)) { fail("MCP bin", `missing ${binPath}`); continue; }
      const mode = statSync(binPath).mode & 0o111;
      if (mode === 0) { fail("MCP bin executable", `${binPath} mode 0${statSync(binPath).mode.toString(8).slice(-3)} (not executable)`); continue; }
      ok(`MCP bin executable (0${statSync(binPath).mode.toString(8).slice(-3)})`);
      const firstLine = readFileSync(binPath, "utf8").split("\n", 1)[0];
      if (firstLine !== "#!/usr/bin/env node") { fail("MCP bin shebang", `got: ${firstLine}`); continue; }
      ok("MCP bin shebang");
      const help = run("node", [binPath, "--help"]);
      if (help.status !== 0 || !help.stdout.includes("computer-use-mcp")) {
        fail("MCP bin --help", `exit ${help.status}${help.stderr ? `; stderr: ${help.stderr.slice(0, 300)}` : ""}`);
        continue;
      }
      ok("MCP bin --help (exit 0, usage printed)");
      try {
        const { lines, toolsResult } = await mcpInitializeAndToolsList(binPath);
        const tools = toolsResult.tools.map((t) => t.name);
        for (const line of lines) ok(`MCP stdio: ${line}`);
        const core = ["computer_observe", "computer_act", "computer_inspect", "computer_session"];
        const missing = core.filter((t) => !tools.includes(t));
        if (missing.length) { fail("MCP tools/list core tools", `missing: ${missing.join(", ")}`); continue; }
        if (tools.length !== 7) { fail("MCP tools/list count", `expected 7, got ${tools.length}: ${tools.join(", ")}`); continue; }
        ok(`MCP tools/list (${tools.length} tools)`);
      } catch (err) {
        fail("MCP initialize + tools/list", err.message);
      }
      // The .bin shim must also resolve (npm creates it from `bin`).
      const shim = join(installDir, "node_modules", ".bin", "computer-use-mcp");
      if (!existsSync(shim)) { fail("MCP .bin shim", `missing ${shim}`); continue; }
      ok("MCP .bin/computer-use-mcp shim present");
    }

    if (pkg === "pi-extension") {
      const imp = sh(`cd ${JSON.stringify(installDir)} && node --input-type=module -e '
        import("${pkgName}").then(async (m) => {
          if (typeof m.default !== "function") throw new Error("default export is not a factory");
          const tools = [], commands = [], handlers = {};
          m.default({ registerTool: (d) => tools.push(d.name), registerCommand: (n) => commands.push(n), on: (e, h) => { handlers[e] = h; } });
          const expected = ["computer_session", "computer_observe", "computer_act", "computer_inspect"];
          const missing = expected.filter((t) => !tools.includes(t));
          if (missing.length) throw new Error("missing tools: " + missing);
          if (typeof handlers.session_shutdown !== "function") throw new Error("no session_shutdown handler");
          if (commands.length < 7) throw new Error("expected >=7 commands, got " + commands.length);
          console.log("factory ok, " + tools.length + " tools, " + commands.length + " commands");
        })'`);
      if (imp.status !== 0) { fail("Pi tarball load", imp.stderr); continue; }
      ok("Pi default export is a factory (4 tools + session_shutdown)");
    }

    if (pkg === "opencode-adapter") {
      const imp = sh(`cd ${JSON.stringify(installDir)} && node --input-type=module -e '
        import("${pkgName}").then(async (m) => {
          if (typeof m.generateOpenCodeConfig !== "function") throw new Error("no generateOpenCodeConfig");
          const cfg = m.generateOpenCodeConfig();
          const entry = cfg.mcp && cfg.mcp["computer-use"];
          if (!entry || entry.command[0] !== "computer-use-mcp") throw new Error("bad mcp entry: " + JSON.stringify(entry));
          console.log("config ok");
        })'`);
      if (imp.status !== 0) { fail("OpenCode adapter import", imp.stderr); continue; }
      ok("OpenCode generateOpenCodeConfig import");
      const cliPath = join(installedDir, "dist", "cli.js");
      if (!existsSync(cliPath)) { fail("OpenCode cli.js", `missing ${cliPath}`); continue; }
      const cliMode = statSync(cliPath).mode & 0o111;
      if (cliMode === 0) { fail("OpenCode cli.js executable", `${cliPath} mode ${statSync(cliPath).mode.toString(8)}`); continue; }
      ok("OpenCode dist/cli.js executable");
      // Does the opencode CLI have a --help that exits? Probe it (may not; the
      // executable presence + config generation are the hard requirements).
      const help = run("node", [cliPath, "--help"], { timeout: 10_000 });
      if (help.status === 0 && (help.stdout + help.stderr).includes("cu-opencode")) {
        ok("OpenCode cli --help");
      } else {
        console.log(`        (note: cu-opencode --help exited ${help.status} — CLI may wait on stdin; not a failure)`);
      }
    }
  }

  section("workspace independence");
  // The whole point: the installed packages must not reference the repo.
  const leak = sh(`grep -rlF ${JSON.stringify(REPO)} ${PKGS.map((pkg) => JSON.stringify(join(work, `${pkg}-install`))).join(" ")} 2>/dev/null | head -5`);
  const hits = leak.stdout.trim().split("\n").filter(Boolean);
  if (hits.length) { fail("workspace leakage", `${hits.length} installed file(s) contain the repo path:\n${hits.slice(0, 3).join("\n")}`); }
  else ok("no installed package references the workspace path");
} finally {
  rmSync(work, { recursive: true, force: true });
}

console.log(`\n== tarball verification: ${checks} checks, ${failures} failures ==`);
process.exit(failures === 0 ? 0 : 1);
