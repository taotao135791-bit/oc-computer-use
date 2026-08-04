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
import { readFile, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";

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
 */
export function generateOpenCodeConfig(existing: Record<string, unknown> = {}): Record<string, unknown> {
  const mcp = { ...(typeof existing.mcp === "object" && existing.mcp !== null ? (existing.mcp as Record<string, unknown>) : {}) };
  mcp["computer-use"] = mcpConfigFragment();
  return { ...existing, mcp };
}

/**
 * Write (or merge into) an OpenCode config file. Returns the path written
 * and whether the file existed before.
 */
export async function writeOpenCodeConfig(
  path: string,
  existing?: Record<string, unknown>,
): Promise<{ path: string; existed: boolean; merged: boolean }> {
  const existed = existsSync(path);
  let base: Record<string, unknown>;
  if (existing) {
    base = existing;
  } else if (existed) {
    try {
      base = JSON.parse(await readFile(path, "utf8"));
    } catch {
      throw new Error(`cannot parse existing config at ${path}`);
    }
  } else {
    base = {};
  }
  const hadEntry = Boolean((base.mcp as Record<string, unknown> | undefined)?.["computer-use"]);
  const config = generateOpenCodeConfig(base);
  await writeFile(path, `${JSON.stringify(config, null, 2)}\n`);
  return { path, existed, merged: hadEntry };
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
