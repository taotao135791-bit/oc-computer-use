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

export const SERVER_NAME = "computer-use";
export const SERVER_VERSION = "0.1.0";

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

/**
 * Build the MCP server. `client` may be injected for tests; by default a
 * client is created for the daemon socket when the first request arrives.
 */
export function createComputerUseServer(client?: ComputerUseClient): McpServer {
  const server = new McpServer({
    name: SERVER_NAME,
    version: SERVER_VERSION,
  });

  let lazyClient: ComputerUseClient | undefined = client;

  async function getClient(): Promise<ComputerUseClient> {
    if (lazyClient) return lazyClient;
    lazyClient = await connect({
      socketPath: process.env.COMPUTER_USE_SOCKET,
    });
    return lazyClient;
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
        "paused or when the user has taken over.",
      inputSchema: z.object({
        action: z
          .enum(["start", "status", "pause", "resume", "stop", "takeover", "release"])
          .describe("Session action"),
        session_id: z.string().optional().describe("Required except for start/status"),
        display_id: z.string().optional().describe("Display to capture (defaults to primary)"),
      }),
    },
    async ({ action, session_id, display_id }) => {
      try {
        const c = await getClient();
        const result = await c.session(action, { session_id, display_id });
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
        "stops at the next safe boundary.",
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
      description: "List all recorded session traces with entry counts and sizes.",
      inputSchema: z.object({}),
    },
    async () => {
      try {
        const c = await getClient();
        const { traces } = await c.traceList();
        if (traces.length === 0) return { content: [textBlock("no traces recorded")] };
        const lines = traces.map(
          (t) =>
            `${t.session_id}\t${t.entries} entries\t${t.bytes} bytes\t${t.started_at}`,
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
async function main(): Promise<void> {
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
