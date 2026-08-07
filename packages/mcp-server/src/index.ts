// MCP server for the computer-use runtime.
//
// Speaks the Model Context Protocol over stdio and drives the daemon through
// @computer-use/sdk. Every tool that produces a screenshot returns an MCP
// *image content block* (base64 + mimeType) so vision-capable clients see the
// actual desktop, plus a text block with the structured metadata
// (frame_id, dimensions, active application, …).
//
// Run with: `node dist/index.js` (binary: `computer-use-mcp`).
// Config via environment:
//   COMPUTER_USE_SOCKET   — daemon socket path (default ~/.computer-use/runtime.sock)

import { chmodSync, mkdirSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";
import { z } from "zod";

import {
  connect,
  ComputerUseError,
  type ComputerUseClient,
  type ComputerAction,
  type ObserveResult,
  type SessionResult,
} from "@computer-use/sdk";

// Wire types generated from the Rust protocol source of truth (never hand-edited).
export * from "./generated/protocol.js";

export const SERVER_NAME = "computer-use";
export const SERVER_VERSION = "0.2.0-alpha.1";

type ContentBlock = CallToolResult["content"][number];

/** MCP image content block for a base64 screenshot. */
function imageBlock(base64: string, mimeType: string): ContentBlock {
  return { type: "image", data: base64, mimeType };
}

function textBlock(text: string): ContentBlock {
  return { type: "text", text };
}

/** Map a ComputerUseError into a helpful text block. */
function errorBlock(err: unknown): ContentBlock[] {
  if (err instanceof ComputerUseError) {
    return [
      textBlock(
        `ERROR ${err.code}: ${err.message}${err.data && typeof err.data === "object" && Object.keys(err.data as object).length > 0 ? `\n${JSON.stringify(err.data, null, 2)}` : ""}`,
      ),
    ];
  }
  return [textBlock(`ERROR: ${(err as Error).message ?? String(err)}`)];
}

/** Text summary of an observe result (without the image). */
function observeSummary(f: ObserveResult): string {
  const lines = [
    `session_id: ${f.session_id}`,
    `frame_id: ${f.frame_id}`,
    `size: ${f.width}x${f.height}`,
    `display_id: ${f.display_id}`,
    `scale_factor: ${f.scale_factor}`,
    `captured_at: ${f.captured_at}`,
  ];
  if (f.active_application) lines.push(`active_application: ${f.active_application}`);
  if (f.active_window) lines.push(`active_window: ${f.active_window}`);
  return lines.join("\n");
}

function sessionSummary(s: SessionResult): string {
  return [
    `session_id: ${s.session_id}`,
    `state: ${s.state}`,
    `paused: ${s.paused}`,
    `user_takeover: ${s.user_takeover}`,
    `lock_held: ${s.lock_held}`,
    `display_id: ${s.display_id}`,
    `started_by: ${s.started_by}`,
    ...(s.current_frame_id ? [`current_frame_id: ${s.current_frame_id}`] : []),
    ...(s.trace_dir ? [`trace_dir: ${s.trace_dir}`] : []),
    ...(s.message ? [`message: ${s.message}`] : []),
  ].join("\n");
}

export interface McpServerOptions {
  /**
   * Stop the session this server started when the process exits (default
   * true). Only sessions this server *owns* are stopped — never another
   * client's session: the daemon would refuse a stop without the token
   * anyway, and the SDK only holds a token for sessions it started.
   */
  stopOwnedSessionOnExit?: boolean;
}

/**
 * Build the MCP server. `client` may be injected for tests; by default a
 * client is created for the daemon socket when the first request arrives.
 */
export function createComputerUseServer(
  client?: ComputerUseClient,
  opts: McpServerOptions = {},
): McpServer {
  const stopOwnedSessionOnExit = opts.stopOwnedSessionOnExit ?? true;
  const server = new McpServer({
    name: SERVER_NAME,
    version: SERVER_VERSION,
  });

  let lazyClient: ComputerUseClient | undefined = client;
  // True while this server owns the active session (it issued the start). The
  // SDK holds the control token only for sessions it started, so on exit this
  // server stops exactly what it owns — never a session another client runs.
  let ownsSession = false;

  /**
   * Identity this server reports when it starts a session. The daemon records
   * it on the session as the owner — the only client allowed to stop it.
   */
  const MCP_CLIENT_INFO = {
    client_id: "mcp-server",
    client_name: "Computer Use MCP",
    client_instance_id: `mcp-${process.pid}-${Math.random().toString(36).slice(2, 8)}`,
  };

  async function getClient(): Promise<ComputerUseClient> {
    if (lazyClient) return lazyClient;
    lazyClient = await connect({
      socketPath: process.env.COMPUTER_USE_SOCKET,
      clientInfo: MCP_CLIENT_INFO,
    });
    return lazyClient;
  }

  /**
   * Persist the tokens issued by a `start` response to the same 0600
   * credential store the CLI uses (`~/.local/state/oc-computer-use/credentials/<sid>.json`).
   * The daemon issues the tokens exactly once; the benchmark runner and any
   * later CLI read the session's trace through this record (same-UID, mode
   * 0600, never printed). Without it, traces of MCP-driven sessions were
   * unreadable — the runner could not tell which credential authorized the
   * session. Failures are non-fatal: the session still works; trace reads
   * just fall back to the CLI-created record.
   */
  function persistCredential(result: SessionResult, clientInstanceId: string): void {
    try {
      if (!result.session_id || !result.control_token) return;
      const dir = join(homedir(), ".local", "state", "oc-computer-use", "credentials");
      mkdirSync(dir, { recursive: true, mode: 0o700 });
      chmodSync(dir, 0o700);
      const file = join(dir, `${result.session_id}.json`);
      writeFileSync(
        file,
        JSON.stringify(
          {
            session_id: result.session_id,
            control_token: result.control_token,
            observation_token: result.observation_token ?? "",
            client_instance_id: clientInstanceId,
            created_at: result.created_at,
            format_version: 1,
          },
          null,
          2,
        ) + "\n",
      );
      chmodSync(file, 0o600);
    } catch {
      /* non-fatal: see doc comment */
    }
  }

  if (stopOwnedSessionOnExit) {
    // Best-effort cleanup on termination: stop only the session this server
    // owns. `computer_cancel`/stop requests from other clients never touch
    // this session (the daemon verifies the token per request), so the worst
    // case on exit is an abandoned-but-still-owned session, which the owner
    // (or `cu session stop`) can stop later.
    let shuttingDown = false;
    const stopOwned = async (): Promise<void> => {
      if (shuttingDown) return;
      shuttingDown = true;
      const c = lazyClient;
      if (!ownsSession || !c) return;
      const cred = c.getSessionCredential();
      if (!cred) return;
      try {
        await Promise.race([
          c.session("stop", { session_id: cred.sessionId }),
          new Promise((_, reject) => setTimeout(() => reject(new Error("stop timed out")), 3000)),
        ]);
      } catch {
        // Daemon gone or session already stopped — nothing left to do.
      }
    };
    const onSignal = (signal: string): void => {
      void stopOwned().finally(() => process.exit(0));
    };
    process.on("SIGINT", () => onSignal("SIGINT"));
    process.on("SIGTERM", () => onSignal("SIGTERM"));
  }

  // --- computer_session ----------------------------------------------------
  server.registerTool(
    "computer_session",
    {
      title: "Session lifecycle",
      description:
        "Start, inspect, pause, resume, stop, or release a computer-use session. " +
        "`status` resolves the currently active session (if any). " +
        "Sessions gate access to the keyboard and mouse — actions are rejected while " +
        "paused or when the user has taken over. " +
        "Only the client that started a session owns it: knowing a session id grants " +
        "no control, and `stop` is honored only for the owner (the daemon verifies a " +
        "control token that is issued once at start).",
      inputSchema: z.object({
        action: z
          .enum(["start", "status", "pause", "resume", "stop", "takeover", "release"])
          .describe("Session action"),
        session_id: z.string().optional().describe("Required except for start/status"),
        display_id: z.string().optional().describe("Display to capture (defaults to primary)"),
        // Round 9 / P0-5: expose session isolation configuration. `target`
        // scopes the session to an app/window (Chrome, TextEdit); the
        // pointer policy decides when the real cursor may be borrowed;
        // focus policy defaults to strict (no focus stealing).
        target: z
          .object({
            bundle_id: z.string().optional().describe("App bundle id, e.g. com.google.Chrome"),
            pid: z.number().int().optional().describe("App process id"),
            window_id: z.number().int().optional().describe("Window id (CGWindowNumber)"),
          })
          .optional()
          .describe("Scope the session to one app/window"),
        pointer_policy: z
          .enum(["isolated_only", "isolated_preferred", "physical_allowed"])
          .optional()
          .describe(
            "Pointer isolation: isolated_only never touches the real cursor; " +
            "isolated_preferred prefers isolation and only uses physical fallback " +
            "when explicitly allowed; physical_allowed may borrow the cursor",
          ),
        focus_policy: z
          .enum(["strict", "activate_target"])
          .optional()
          .describe(
            "Keyboard focus policy: strict rejects type/key when focus is not on the " +
            "target (never steals foreground); activate_target is experimental/unsupported",
          ),
      }),
    },
    async ({ action, session_id, display_id }) => {
      try {
        const c = await getClient();
        const result = await c.session(action, { session_id, display_id, target, pointer_policy, focus_policy });
        // Ownership follows the token: the daemon issued one in this response,
        // so this server is the owner and may (should) stop it on exit.
        if (action === "start" && result.control_token) {
          ownsSession = true;
          persistCredential(result, MCP_CLIENT_INFO.client_instance_id);
        }
        if (action === "stop" && ownsSession) ownsSession = false;
        return { content: [textBlock(sessionSummary(result))] };
      } catch (err) {
        return { content: errorBlock(err), isError: true };
      }
    },
  );

  // --- computer_observe ----------------------------------------------------
  server.registerTool(
    "computer_observe",
    {
      title: "Observe the screen",
      description:
        "Capture the current desktop. Returns the screenshot as an image block plus " +
        "a text block with frame_id, dimensions, display id, scale factor and the " +
        "active application. Use the frame_id as the reference for computer_act " +
        "coordinates — a stale frame is rejected.",
      inputSchema: z.object({
        session_id: z.string().optional(),
        include_cursor: z.boolean().optional().describe("Draw the cursor into the image"),
        max_width: z
          .number()
          .int()
          .min(64)
          .max(2880)
          .optional()
          .describe("Downscale so the image width does not exceed this"),
        image_format: z.enum(["png", "jpeg"]).optional(),
        include_image: z
          .boolean()
          .optional()
          .describe("Include the image block (default true for vision clients)"),
      }),
    },
    async (params) => {
      try {
        const c = await getClient();
        const frame = await c.observe({ ...params, include_image: true });
        const blocks: ContentBlock[] = [textBlock(observeSummary(frame))];
        if (frame.image_base64 && params.include_image !== false) {
          blocks.push(imageBlock(frame.image_base64, frame.image_mime_type));
        }
        return { content: blocks };
      } catch (err) {
        return { content: errorBlock(err), isError: true };
      }
    },
  );

  // --- computer_act --------------------------------------------------------
  // Structured (non-JSON-string) action schema: a zod discriminated union that
  // mirrors the runtime's serde tagged union exactly (tag `type`, snake_case
  // variants). The MCP input schema is generated from this zod schema, so the
  // model receives the real field list per action type — no JSON string
  // inside JSON, no hand-rolled parsing on this side.

  const coordinateSpace = z
    .enum(["normalized_1000", "image_pixels"])
    .default("normalized_1000")
    .describe("Coordinate space (default normalized_1000; image treated as a 1000x1000 canvas)");

  const pointSchema = z.object({
    x: z.number().min(0).describe("X in the coordinate space"),
    y: z.number().min(0).describe("Y in the coordinate space"),
  });

  const actionSchemas = {
    click: z.object({
      type: z.literal("click"),
      x: z.number().min(0).describe("Click X"),
      y: z.number().min(0).describe("Click Y"),
      button: z.enum(["left", "right", "middle"]).default("left"),
      coordinate_space: coordinateSpace,
    }),
    double_click: z.object({
      type: z.literal("double_click"),
      x: z.number().min(0).describe("Click X"),
      y: z.number().min(0).describe("Click Y"),
      button: z.enum(["left", "right", "middle"]).default("left"),
      coordinate_space: coordinateSpace,
    }),
    move: z.object({
      type: z.literal("move"),
      x: z.number().min(0).describe("Move target X"),
      y: z.number().min(0).describe("Move target Y"),
      coordinate_space: coordinateSpace,
      duration_ms: z.number().int().min(0).optional().describe("Animation duration in ms"),
    }),
    type: z.object({
      type: z.literal("type"),
      text: z
        .string()
        .max(10_000)
        .describe("Text to type; logged redacted by default"),
      method: z
        .enum(["keyboard", "clipboard"])
        .default("keyboard")
        .describe("Input method (default keyboard)"),
    }),
    key: z.object({
      type: z.literal("key"),
      keys: z
        .array(z.string())
        .min(1)
        .max(16)
        .describe("Key names, e.g. [\"return\"] or [\"cmd\", \"c\"]"),
    }),
    scroll: z.object({
      type: z.literal("scroll"),
      x: z.number().min(0).optional().describe("Scroll position X (optional)"),
      y: z.number().min(0).optional().describe("Scroll position Y (optional)"),
      delta_x: z.number().optional().describe("Horizontal scroll delta"),
      delta_y: z.number().optional().describe("Vertical scroll delta"),
      coordinate_space: coordinateSpace,
    }),
    drag: z.object({
      type: z.literal("drag"),
      from: pointSchema.describe("Drag start"),
      to: pointSchema.describe("Drag end"),
      coordinate_space: coordinateSpace,
      duration_ms: z.number().int().min(0).optional().describe("Drag duration in ms"),
    }),
    wait: z.object({
      type: z.literal("wait"),
      duration_ms: z.number().int().min(1).max(600_000).describe("Milliseconds to wait"),
    }),
  } as const;

  const actionUnion = z.discriminatedUnion("type", [
    actionSchemas.click,
    actionSchemas.double_click,
    actionSchemas.move,
    actionSchemas.type,
    actionSchemas.key,
    actionSchemas.scroll,
    actionSchemas.drag,
    actionSchemas.wait,
  ]);

  server.registerTool(
    "computer_act",
    {
      title: "Execute computer actions",
      description:
        "Execute a batch of 1-50 actions on the frame described by frame_id. Each action is " +
        "a structured object discriminated on `type`: click, double_click, move, type (with " +
        "text), key (with keys), scroll (delta_x/delta_y), drag (from/to), wait (duration_ms). " +
        "Coordinates are in normalized_1000 by default (the image treated as a 1000x1000 " +
        "canvas); image_pixels uses the frame's raw pixel grid. The frame must be the most " +
        "recent observe — acting on a stale frame is rejected with STALE_FRAME.",
      inputSchema: z.object({
        session_id: z.string(),
        frame_id: z.string().describe("frame_id from the most recent computer_observe"),
        actions: z
          .array(actionUnion)
          .min(1)
          .max(50)
          .describe("Ordered action batch, executed in sequence"),
        wait_policy: z.enum(["none", "fixed", "until_stable"]).optional(),
        fixed_wait_ms: z.number().int().min(0).optional(),
        return_screenshot: z.boolean().optional(),
      }),
    },
    async ({ session_id, frame_id, actions, wait_policy, fixed_wait_ms, return_screenshot }) => {
      try {
        const c = await getClient();
        const result = await c.act({
          session_id,
          frame_id,
          actions: actions as ComputerAction[],
          wait_policy,
          fixed_wait_ms,
          return_screenshot: return_screenshot ?? true,
        });
        const lines = [
          `executed: ${result.executed}`,
          `screen_changed: ${result.screen_changed}`,
          `stable: ${result.stable}`,
          ...(result.next_frame_id ? [`next_frame_id: ${result.next_frame_id}`] : []),
          ...result.action_results.map(
            (r) => `action[${r.index}]: ${r.status} (${r.duration_ms}ms)${r.error ? ` — ${r.error}` : ""}`,
          ),
        ];
        const blocks: ContentBlock[] = [textBlock(lines.join("\n"))];
        if (result.screenshot?.image_base64) {
          blocks.push(imageBlock(result.screenshot.image_base64, result.screenshot.image_mime_type));
        }
        return { content: blocks };
      } catch (err) {
        return { content: errorBlock(err), isError: true };
      }
    },
  );

  // --- computer_inspect ----------------------------------------------------
  server.registerTool(
    "computer_inspect",
    {
      title: "Inspect a region of the screen",
      description:
        "Crop a region from the last frame and return it as an image block, with the " +
        "mapping needed to translate inspect-relative coordinates back to global " +
        "desktop coordinates (global_origin, normalized_1000_origin). The region is a " +
        "structured object, not a JSON string.",
      inputSchema: z.object({
        session_id: z.string(),
        frame_id: z.string(),
        region: z
          .object({
            x: z.number().min(0).describe("Region left in the frame's coordinate space"),
            y: z.number().min(0).describe("Region top in the frame's coordinate space"),
            width: z.number().min(1).describe("Region width"),
            height: z.number().min(1).describe("Region height"),
            coordinate_space: coordinateSpace.describe(
              "Coordinate space of x/y/width/height (default normalized_1000)",
            ),
          })
          .describe("Rectangle to crop from the frame"),
        scale: z
          .number()
          .int()
          .min(1)
          .max(8)
          .optional()
          .describe("Integer zoom applied to the crop"),
      }),
    },
    async (p) => {
      try {
        const c = await getClient();
        const result = await c.inspect({
          session_id: p.session_id,
          frame_id: p.frame_id,
          region: p.region,
          scale: p.scale,
        });
        const blocks: ContentBlock[] = [
          textBlock(
            [
              `frame_id: ${result.frame_id}`,
              `crop: ${result.width}x${result.height}`,
              `global_origin: ${result.mapping.global_origin.join(",")}`,
              `normalized_1000_origin: ${result.mapping.normalized_1000_origin.join(",")}`,
            ].join("\n"),
          ),
          imageBlock(result.image_base64, result.image_mime_type),
        ];
        return { content: blocks };
      } catch (err) {
        return { content: errorBlock(err), isError: true };
      }
    },
  );

  // --- computer_cancel -----------------------------------------------------
  server.registerTool(
    "computer_cancel",
    {
      title: "Cancel an in-flight act",
      description:
        "Cancel the currently executing computer_act batch for a session. The batch " +
        "stops at the next safe boundary. Cancelling is a mutating operation: the " +
        "daemon verifies the session's control token, which the SDK holds only for " +
        "sessions this server started — cancelling another client's session is refused.",
      inputSchema: z.object({ session_id: z.string() }),
    },
    async ({ session_id }) => {
      try {
        const c = await getClient();
        const result = await c.cancel({ session_id });
        return { content: [textBlock(`cancelled: ${result.cancelled}`)] };
      } catch (err) {
        return { content: errorBlock(err), isError: true };
      }
    },
  );

  // --- trace_list / trace_get ----------------------------------------------
  server.registerTool(
    "trace_list",
    {
      title: "List recorded traces",
      description:
        "List the recorded trace of one session with entry counts and sizes. " +
        "Session-scoped: the session_id must be given, and the daemon requires " +
        "that session's observation credential (held by this server when the " +
        "session belongs to it) — traces of other sessions are never revealed.",
      inputSchema: z.object({
        session_id: z.string().optional().describe("Defaults to the current session"),
      }),
    },
    async ({ session_id }) => {
      try {
        const c = await getClient();
        const cred = c.getSessionCredential();
        const sid = session_id ?? cred?.sessionId;
        if (!sid) {
          return {
            content: [
              textBlock("no session: pass session_id, or start a session first"),
            ],
            isError: true,
          };
        }
        const { traces } = await c.traceList(sid);
        if (traces.length === 0) return { content: [textBlock("no traces recorded")] };
        const lines = traces.map(
          (t) =>
            `${t.session_id}\t${t.event_count} events\t${t.size_bytes} bytes\t${t.created_at}`,
        );
        return { content: [textBlock(lines.join("\n"))] };
      } catch (err) {
        return { content: errorBlock(err), isError: true };
      }
    },
  );

  server.registerTool(
    "trace_get",
    {
      title: "Read a session trace",
      description:
        "Read the JSONL trace of a session. Type action texts are redacted by default " +
        "(text_redacted: true).",
      inputSchema: z.object({ session_id: z.string() }),
    },
    async ({ session_id }) => {
      try {
        const c = await getClient();
        const entries = await c.traceGet(session_id);
        const body = entries
          .map((e) => `${e.seq}\t${e.event}\t${JSON.stringify(e.result ?? e.action ?? "")}`)
          .join("\n");
        return { content: [textBlock(body.slice(0, 16 * 1024))] };
      } catch (err) {
        return { content: errorBlock(err), isError: true };
      }
    },
  );

  return server;
}

/** Entry point: run on stdio until the parent closes the pipe. */
export async function main(): Promise<void> {
  const argv = process.argv.slice(2);
  if (argv.includes("--help") || argv.includes("-h")) {
    // `--help` prints usage and exits — it must never block on stdio (an
    // MCP server with no stdin would otherwise hang forever).
    process.stdout.write(
      [
        `computer-use-mcp ${SERVER_VERSION} — MCP server for the computer-use runtime`,
        "",
        "Speaks the Model Context Protocol over stdio and drives the daemon",
        "through @computer-use/sdk. Requires a running daemon (`cu daemon start`).",
        "",
        "Usage:",
        "  computer-use-mcp               serve MCP over stdio (default)",
        "  computer-use-mcp --help        print this help and exit",
        "",
        "Environment:",
        "  COMPUTER_USE_SOCKET            daemon socket path",
        "                                 (default ~/.computer-use/runtime.sock)",
        "",
        "Tools: computer_session, computer_observe, computer_act,",
        "       computer_inspect, computer_cancel, trace_list, trace_get",
        "",
      ].join("\n") + "\n",
    );
    return;
  }
  const server = createComputerUseServer();
  const transport = new StdioServerTransport();
  await server.connect(transport);
}

// Allow `import` of the module for tests without starting the server.
// Compare via pathToFileURL: import.meta.url percent-encodes non-ASCII
// characters in the path while process.argv[1] is the raw UTF-8 string.
import { pathToFileURL } from "node:url";
const entry = process.argv[1];
if (entry && import.meta.url === pathToFileURL(entry).href) {
  main().catch((err) => {
    console.error(`computer-use MCP server failed: ${err.message}`);
    process.exit(1);
  });
}
