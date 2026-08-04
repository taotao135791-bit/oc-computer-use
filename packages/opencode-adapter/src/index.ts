// OpenCode companion tooling for the computer-use runtime.
//
// OpenCode itself consumes the runtime through the **MCP server**
// (packages/mcp-server, binary `computer-use-mcp`) using OpenCode's official
// config format:
//
//   {
//     "$schema": "https://opencode.ai/config.json",
//     "mcp": {
//       "computer-use": {
//         "type": "local",
//         "command": ["computer-use-mcp"],
//         "enabled": true
//       }
//     }
//   }
//
// This package is the companion CLI (`cu-opencode`): it generates that
// config (`setup`), inspects daemon health and the active session
// (`status`), checks the environment (`doctor`), and cleans up stray
// sessions (`session cleanup`). It deliberately does **not** re-implement
// the tools as an OpenCode plugin — MCP is the single supported wiring.

import { execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { copyFile, readFile, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";

import {
  applyEdits,
  modify,
  parse,
  type ModificationOptions,
  type ParseError,
} from "jsonc-parser";

import {
  connect,
  ComputerUseError,
  defaultSocketPath,
  type ComputerUseClient,
  type Health,
  type SessionResult,
} from "@computer-use/sdk";

// ---------------------------------------------------------------------------
// Config generation
// ---------------------------------------------------------------------------

/** The MCP server binary installed by @computer-use/mcp-server. */
export const MCP_BIN = "computer-use-mcp";

/** MCP server entry (the value under `mcp.computer-use`). */
export function mcpConfigFragment(): Record<string, unknown> {
  return {
    type: "local",
    command: [MCP_BIN],
    enabled: true,
  };
}

/**
 * Build an opencode config object with the computer-use MCP server merged
 * in. Existing keys (other MCP servers, tools, agent config, ...) are kept;
 * an existing `computer-use` entry is replaced with the current fragment.
 * (For writing real config files use `writeOpenCodeConfig`, which preserves
 * comments and trailing commas; this object form backs `--print`.)
 */
export function generateOpenCodeConfig(existing: Record<string, unknown> = {}): Record<string, unknown> {
  const mcp = { ...(typeof existing.mcp === "object" && existing.mcp !== null ? (existing.mcp as Record<string, unknown>) : {}) };
  mcp["computer-use"] = mcpConfigFragment();
  return { ...existing, mcp };
}

/** New nodes are inserted with 2-space indentation; the file's own style for
 * everything else is left untouched. */
const MODIFY_OPTIONS: ModificationOptions = {
  formattingOptions: { insertSpaces: true, tabSize: 2 },
};

/**
 * Merge the computer-use MCP entry into a JSONC config document. Works on
 * text via jsonc-parser's edit API, so comments, trailing commas and the
 * surrounding formatting all survive. Only `mcp["computer-use"]` is created
 * or replaced — other MCP servers, providers, models, plugins, permissions,
 * agents and tools are never touched.
 *
 * Returns the merged text, whether it actually changed, and whether an
 * entry already existed. Throws on unparseable input (a corrupt config is
 * never silently rewritten).
 */
export function mergeOpenCodeConfigText(
  configText: string,
): { text: string; changed: boolean; hadEntry: boolean } {
  const errors: ParseError[] = [];
  const root: unknown = parse(configText, errors, {
    disallowComments: false,
    allowTrailingComma: true,
    allowEmptyContent: true,
  });
  if (errors.length > 0) {
    throw new Error(
      `cannot parse existing config (not valid JSON/JSONC): ${errors.map((e) => e.error).join("; ")}`,
    );
  }
  const mcp = (root as { mcp?: Record<string, unknown> } | undefined)?.mcp;
  const hadEntry = Boolean(mcp?.["computer-use"]);
  // jsonc-parser inserts into an empty document as if it were `{}`.
  const edits = modify(configText || "{}", ["mcp", "computer-use"], mcpConfigFragment(), MODIFY_OPTIONS);
  const text = applyEdits(configText || "{}", edits);
  return { text, changed: text !== (configText || "{}"), hadEntry };
}

/** Backup filename: `<config>.backup-<timestamp>`. */
function backupTimestamp(): string {
  return new Date().toISOString().replace(/[:.]/g, "-");
}

/**
 * Write (or merge into) an OpenCode config file. Comments and trailing
 * commas in an existing JSONC file survive; only the computer-use MCP entry
 * is created/updated. The original file is backed up to
 * `<path>.backup-<timestamp>` before the first actual change (never for a
 * no-op or a brand-new file).
 */
export async function writeOpenCodeConfig(
  path: string,
): Promise<{ path: string; existed: boolean; merged: boolean; changed: boolean; backup: string | null }> {
  const existed = existsSync(path);
  const original = existed ? await readFile(path, "utf8") : "";
  const { text, changed, hadEntry } = mergeOpenCodeConfigText(original);
  if (!changed) {
    // Nothing to do — in particular, no backup for a no-op write.
    return { path, existed, merged: hadEntry, changed: false, backup: null };
  }
  let backup: string | null = null;
  if (existed) {
    backup = `${path}.backup-${backupTimestamp()}`;
    await copyFile(path, backup);
  }
  await writeFile(path, text);
  return { path, existed, merged: hadEntry, changed: true, backup };
}

// ---------------------------------------------------------------------------
// Introspection + cleanup
// ---------------------------------------------------------------------------

/** Text summary of daemon health and the current session (for `status`). */
export async function statusText(client: ComputerUseClient): Promise<string> {
  const health: Health = await client.health();
  const lines = [
    `daemon: v${health.version} ${health.ready ? "ready" : "NOT READY"}`,
    `permissions: screen_recording=${health.permissions.screen_recording} accessibility=${health.permissions.accessibility}`,
    `active_sessions: ${health.active_sessions}`,
  ];
  try {
    const s: SessionResult = await client.session("status");
    lines.push(
      `session: ${s.session_id} state=${s.state} paused=${s.paused} takeover=${s.user_takeover} lock=${s.lock_held}`,
      ...(s.current_frame_id ? [`current_frame_id: ${s.current_frame_id}`] : []),
      ...(s.trace_dir ? [`trace_dir: ${s.trace_dir}`] : []),
    );
  } catch (err) {
    if (err instanceof ComputerUseError && err.code === "SESSION_NOT_FOUND") {
      lines.push("session: none");
    } else {
      lines.push(`session: ${(err as Error).message}`);
    }
  }
  return lines.join("\n");
}

/** Stop the active session (if any); idempotent cleanup. */
export async function cleanupSession(client: ComputerUseClient): Promise<{ stopped: boolean; message: string }> {
  try {
    const s: SessionResult = await client.session("status");
    await client.session("stop", { session_id: s.session_id });
    return {
      stopped: true,
      message: `stopped session ${s.session_id} (was ${s.state})`,
    };
  } catch (err) {
    if (err instanceof ComputerUseError && err.code === "SESSION_NOT_FOUND") {
      return { stopped: false, message: "no active session to clean up" };
    }
    throw err;
  }
}

export interface DoctorReport {
  daemon_binary: string | null;
  socket_path: string;
  socket_exists: boolean;
  mcp_binary: string | null;
  daemon_health: Health | null;
  errors: string[];
}

/**
 * Environment check: daemon binary on PATH, socket present, MCP binary on
 * PATH, and (when reachable) daemon health/permissions.
 */
export async function doctor(options: { socketPath?: string } = {}): Promise<DoctorReport> {
  const socketPath = options.socketPath ?? process.env.COMPUTER_USE_SOCKET ?? defaultSocketPath();
  const errors: string[] = [];
  const report: DoctorReport = {
    daemon_binary: null,
    socket_path: socketPath,
    socket_exists: existsSync(socketPath),
    mcp_binary: null,
    daemon_health: null,
    errors,
  };

  for (const name of ["cu", MCP_BIN]) {
    try {
      // `which` semantics without a shell: PATH lookup.
      execFileSync("which", [name], { stdio: "ignore" });
      if (name === "cu") report.daemon_binary = name;
      else report.mcp_binary = name;
    } catch {
      // not on PATH
    }
  }
  if (!report.daemon_binary) {
    errors.push("`cu` binary not found on PATH — install with `cargo install --path crates/cu-cli` or use the daemon binary directly");
  }
  if (!report.mcp_binary) {
    errors.push("`computer-use-mcp` binary not found on PATH — install the @computer-use/mcp-server package");
  }
  if (!report.socket_exists) {
    errors.push(`no socket at ${socketPath} — start the daemon with \`cu daemon start\``);
  } else {
    try {
      const client = await connect({ socketPath });
      report.daemon_health = await client.health();
      client.close();
    } catch (err) {
      errors.push(`daemon at ${socketPath} unreachable: ${(err as Error).message}`);
    }
  }
  return report;
}

/** Human-readable doctor report. */
export function doctorText(r: DoctorReport): string {
  const lines = [
    `cu binary:        ${r.daemon_binary ?? "NOT FOUND"}`,
    `mcp binary:       ${r.mcp_binary ?? "NOT FOUND"}`,
    `socket:           ${r.socket_path} (${r.socket_exists ? "present" : "missing"})`,
  ];
  if (r.daemon_health) {
    lines.push(
      `daemon:           v${r.daemon_health.version} ${r.daemon_health.ready ? "ready" : "NOT READY"} (screen_recording=${r.daemon_health.permissions.screen_recording}, accessibility=${r.daemon_health.permissions.accessibility})`,
    );
  } else {
    lines.push("daemon:           unreachable");
  }
  if (r.errors.length > 0) {
    lines.push("", "Issues found:", ...r.errors.map((e) => `  - ${e}`));
  }
  return lines.join("\n");
}

/** Default OpenCode config location. */
export function defaultOpenCodeConfigPath(): string {
  return join(homedir(), ".config", "opencode", "opencode.json");
}
