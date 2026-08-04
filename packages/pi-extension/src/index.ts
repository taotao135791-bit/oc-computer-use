// Pi extension for the computer-use runtime.
//
// A Pi (pi-coding-agent) extension is a module whose **default export is a
// factory function** receiving the official `ExtensionAPI` (`pi`). This
// extension registers the four computer-use tools (computer_session,
// computer_observe, computer_inspect, computer_act) plus a command set
// (`/computer-status`, `/computer-start`, ...), all backed by the real daemon
// through @computer-use/sdk.
//
// The model **sees the actual desktop**: observe/inspect return real MCP-style
// image content blocks (`{ type: "image", data: <base64>, mimeType }`), never
// an image_path, a base64 length, or a download URL.
//
// Install: copy to `~/.pi/agent/extensions/` (global) or `.pi/extensions/`
// (project-local), then `/reload` in Pi. The daemon socket is read from
// `COMPUTER_USE_SOCKET` (default `~/.computer-use/runtime.sock`).

import { join } from "node:path";

import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { StringEnum } from "@earendil-works/pi-ai";
import { Type, type Static } from "typebox";

import type { ImageContent, TextContent } from "@earendil-works/pi-ai";

import {
  connect,
  ComputerUseError,
  type ComputerUseClient,
  type RequestOptions,
  type SessionAction,
  type SessionResult,
} from "@computer-use/sdk";

// ---------------------------------------------------------------------------
// Daemon connection + session state (module-scoped per Pi runtime instance)
// ---------------------------------------------------------------------------

let client: ComputerUseClient | null = null;
let sessionId: string | null = null;

async function getClient(): Promise<ComputerUseClient> {
  if (!client) {
    client = await connect({ socketPath: process.env.COMPUTER_USE_SOCKET });
  }
  return client;
}

/** Resolve the current session, starting one if none exists. */
async function ensureSession(options?: RequestOptions): Promise<string> {
  if (sessionId) return sessionId;
  const c = await getClient();
  let s: SessionResult;
  try {
    s = await c.session("status", {}, options);
  } catch (err) {
    if (err instanceof ComputerUseError && err.code === "SESSION_NOT_FOUND") {
      s = await c.session("start", {}, options);
    } else {
      throw err;
    }
  }
  sessionId = s.session_id;
  return sessionId;
}

/** Map any thrown SDK error into a Pi tool error (throw → isError: true). */
function toToolError(err: unknown): Error {
  if (err instanceof Error) return err;
  return new Error(String(err));
}

/** Graceful result for a tool call that was already aborted. */
function cancelledResult() {
  return { content: [{ type: "text" as const, text: "Cancelled by user" }], details: {} };
}

// ---------------------------------------------------------------------------
// Schemas (TypeBox — the official Pi schema library)
// ---------------------------------------------------------------------------

const coordinateSpace = StringEnum(["normalized_1000", "image_pixels"], {
  description: "Coordinate space. normalized_1000 treats the image as a 1000x1000 canvas (default); image_pixels uses the raw pixel grid.",
});

const waitPolicy = StringEnum(["none", "fixed", "until_stable"], {
  description: "Post-batch wait policy: none, a fixed wait (fixed_wait_ms), or until the screen stops changing (default none).",
});

/**
 * One action. Deliberately a *flat* object with all fields optional except
 * `type`: nested `anyOf`/`oneOf` unions are rejected by Anthropic's tool
 * schema, so the strict per-type validation happens in `validateAction` with
 * field-level error messages instead.
 */
const actionSchema = Type.Object({
  type: StringEnum(["click", "double_click", "move", "type", "key", "scroll", "drag", "wait"], {
    description: "Action type",
  }),
  x: Type.Optional(Type.Number({ minimum: 0, description: "X in the coordinate space (click/double_click/move/scroll)" })),
  y: Type.Optional(Type.Number({ minimum: 0, description: "Y in the coordinate space (click/double_click/move/scroll)" })),
  button: Type.Optional(StringEnum(["left", "right", "middle"], { description: "Mouse button (default left)" })),
  coordinate_space: Type.Optional(coordinateSpace),
  text: Type.Optional(Type.String({ maxLength: 10_000, description: "Text to type (logged redacted by default)" })),
  method: Type.Optional(StringEnum(["keyboard", "clipboard"], { description: "Text input method (default keyboard)" })),
  keys: Type.Optional(Type.Array(Type.String(), { minItems: 1, maxItems: 16, description: "Key names, e.g. [\"return\"] or [\"cmd\", \"c\"]" })),
  delta_x: Type.Optional(Type.Number({ description: "Horizontal scroll delta" })),
  delta_y: Type.Optional(Type.Number({ description: "Vertical scroll delta" })),
  from: Type.Optional(Type.Object({ x: Type.Number({ minimum: 0 }), y: Type.Number({ minimum: 0 }) })),
  to: Type.Optional(Type.Object({ x: Type.Number({ minimum: 0 }), y: Type.Number({ minimum: 0 }) })),
  duration_ms: Type.Optional(Type.Number({ minimum: 0, description: "Animation duration in ms (move/drag)" })),
});

type RawAction = Static<typeof actionSchema>;

const REQUIRED_FIELDS: Record<string, string[]> = {
  click: ["x", "y"],
  double_click: ["x", "y"],
  move: ["x", "y"],
  type: ["text"],
  key: ["keys"],
  scroll: [],
  drag: ["from", "to"],
  wait: ["duration_ms"],
};

/** Field-level validation of one action; throws with the offending path. */
function validateAction(a: RawAction, index: number): void {
  const missing = (REQUIRED_FIELDS[a.type] ?? []).filter((f) => a[f as keyof RawAction] === undefined);
  if (missing.length > 0) {
    throw new Error(`actions[${index}]: "${a.type}" requires ${missing.join(", ")}`);
  }
}

// ---------------------------------------------------------------------------
// The extension
// ---------------------------------------------------------------------------

export default function computerUseExtension(pi: ExtensionAPI): void {
  // --- Tools ---------------------------------------------------------------

  pi.registerTool({
    name: "computer_session",
    label: "Session lifecycle",
    description:
      "Start, inspect, pause, resume, stop, take over, or release a computer-use session. " +
      "Sessions gate access to the keyboard and mouse: actions are rejected while the " +
      "session is paused or while the user holds control (takeover). `release` returns " +
      "control from the human to the agent; `resume` only works while paused. " +
      "Omit session_id to act on the current session. " +
      "Output: session id, state, paused, user_takeover, lock_held, started_by.",
    promptSnippet: "Start or inspect a computer-use session, or manage pause/takeover state.",
    parameters: Type.Object({
      action: StringEnum(["start", "status", "pause", "resume", "stop", "takeover", "release"], {
        description: "Session action",
      }),
      session_id: Type.Optional(Type.String({ description: "Omit to act on the current session" })),
      display_id: Type.Optional(Type.String({ description: "Display to capture (defaults to primary)" })),
    }),
    async execute(toolCallId, params, signal) {
      if (signal?.aborted) return cancelledResult();
      const c = await getClient();
      const result = await c.session(
        params.action as SessionAction,
        { session_id: params.session_id ?? undefined, display_id: params.display_id ?? undefined },
        { signal },
      );
      sessionId = result.session_id;
      const lines = [
        `session_id: ${result.session_id}`,
        `state: ${result.state}`,
        `paused: ${result.paused}`,
        `user_takeover: ${result.user_takeover}`,
        `lock_held: ${result.lock_held}`,
        `started_by: ${result.started_by}`,
        ...(result.current_frame_id ? [`current_frame_id: ${result.current_frame_id}`] : []),
        ...(result.message ? [`message: ${result.message}`] : []),
      ];
      return {
        content: [{ type: "text", text: lines.join("\n") }],
        details: result,
      };
    },
  });

  pi.registerTool({
    name: "computer_observe",
    label: "Observe the screen",
    description:
      "Capture the current desktop and return it as an image content block (base64 + " +
      "mimeType) the model can see, plus a text summary with frame_id, dimensions, " +
      "display id, scale factor, and the active application. " +
      "The returned frame_id is the reference for computer_act: acting on a stale frame " +
      "is rejected with STALE_FRAME, so re-observe before acting after the screen changed. " +
      "Coordinates in computer_act are normalized_1000 (the image treated as a 1000x1000 " +
      "canvas, top-left origin) unless image_pixels is specified. " +
      "A session is started automatically on first use. " +
      "Use computer_inspect to zoom into a region of this frame.",
    promptSnippet: "Capture the current desktop as an image the model can see.",
    parameters: Type.Object({
      include_cursor: Type.Optional(Type.Boolean({ description: "Draw the cursor into the image" })),
      max_width: Type.Optional(Type.Number({ minimum: 64, maximum: 2880, description: "Downscale so the image width does not exceed this" })),
      image_format: Type.Optional(StringEnum(["png", "jpeg"])),
    }),
    async execute(toolCallId, params, signal) {
      if (signal?.aborted) return cancelledResult();
      const c = await getClient();
      const sid = await ensureSession({ signal });
      const frame = await c.observe(
        {
          session_id: sid,
          include_cursor: params.include_cursor,
          max_width: params.max_width,
          image_format: params.image_format as "png" | "jpeg" | undefined,
          include_image: true,
        },
        { signal },
      );
      if (!frame.image_base64) {
        throw new Error("daemon returned no image for computer.observe");
      }
      const lines = [
        `session_id: ${frame.session_id}`,
        `frame_id: ${frame.frame_id}`,
        `size: ${frame.width}x${frame.height}`,
        `display_id: ${frame.display_id}`,
        `scale_factor: ${frame.scale_factor}`,
        ...(frame.active_application ? [`active_application: ${frame.active_application}`] : []),
        ...(frame.active_window ? [`active_window: ${frame.active_window}`] : []),
        `captured_at: ${frame.captured_at}`,
      ];
      return {
        content: [
          { type: "text", text: lines.join("\n") },
          { type: "image", data: frame.image_base64, mimeType: frame.image_mime_type },
        ],
        details: {
          frame_id: frame.frame_id,
          session_id: frame.session_id,
          width: frame.width,
          height: frame.height,
          display_id: frame.display_id,
          scale_factor: frame.scale_factor,
          active_application: frame.active_application ?? null,
          active_window: frame.active_window ?? null,
          captured_at: frame.captured_at,
        },
      };
    },
  });

  pi.registerTool({
    name: "computer_act",
    label: "Execute computer actions",
    description:
      "Execute a batch of 1-50 actions on the frame referenced by frame_id (the most " +
      "recent computer_observe). Each action is an object with `type` in " +
      "[click, double_click, move, type, key, scroll, drag, wait] plus its fields: " +
      "click/double_click need x, y (button optional, default left); move needs x, y; " +
      "type needs text (method optional); key needs keys (array); scroll uses delta_x/" +
      "delta_y (and optional x, y); drag needs from and to objects {x, y}; wait needs " +
      "duration_ms. Coordinates are normalized_1000 by default; the frame must be the " +
      "most recent observe, otherwise STALE_FRAME is returned — re-observe and retry. " +
      "wait_policy=until_stable waits until the screen stops changing (max 8s) and " +
      "reports the stabilization outcome; return_screenshot=true (default) returns the " +
      "post-batch screenshot as an image block. " +
      "The session must not be paused and the user must not hold control (takeover).",
    promptSnippet: "Execute mouse/keyboard actions on the observed desktop frame.",
    parameters: Type.Object({
      frame_id: Type.String({ description: "frame_id from the most recent computer_observe" }),
      actions: Type.Array(actionSchema, {
        minItems: 1,
        maxItems: 50,
        description: "Ordered action batch, executed in sequence",
      }),
      wait_policy: Type.Optional(waitPolicy),
      fixed_wait_ms: Type.Optional(Type.Number({ minimum: 0, description: "Wait duration when wait_policy=fixed" })),
      return_screenshot: Type.Optional(Type.Boolean({ description: "Return the post-batch screenshot as an image block (default true)" })),
    }),
    async execute(toolCallId, params, signal) {
      if (signal?.aborted) return cancelledResult();
      params.actions.forEach(validateAction);
      // Fill the wire-required defaults the model may have omitted.
      const actions = params.actions.map((a) => {
        const n: Record<string, unknown> = { ...a };
        if (["click", "double_click", "move", "scroll", "drag"].includes(a.type)) {
          n.coordinate_space = a.coordinate_space ?? "normalized_1000";
        }
        if (a.type === "click" || a.type === "double_click") {
          n.button = a.button ?? "left";
        }
        if (a.type === "type") {
          n.method = a.method ?? "keyboard";
        }
        return n;
      });
      const c = await getClient();
      const sid = await ensureSession({ signal });
      const result = await c.act(
        {
          session_id: sid,
          frame_id: params.frame_id,
          actions: actions as never,
          wait_policy: params.wait_policy as "none" | "fixed" | "until_stable" | undefined,
          fixed_wait_ms: params.fixed_wait_ms,
          return_screenshot: params.return_screenshot ?? true,
        },
        { signal },
      );
      const lines = [
        `executed: ${result.executed}`,
        `screen_changed: ${result.screen_changed}`,
        `stable: ${result.stable}`,
        ...(result.next_frame_id ? [`next_frame_id: ${result.next_frame_id}`] : []),
        ...result.action_results.map(
          (r) => `action[${r.index}]: ${r.status} (${r.duration_ms}ms)${r.error ? ` — ${r.error}` : ""}`,
        ),
      ];
      const content: (TextContent | ImageContent)[] = [{ type: "text", text: lines.join("\n") }];
      if (result.screenshot?.image_base64) {
        content.push({
          type: "image",
          data: result.screenshot.image_base64,
          mimeType: result.screenshot.image_mime_type,
        });
      }
      return {
        content,
        details: {
          executed: result.executed,
          screen_changed: result.screen_changed,
          stable: result.stable,
          next_frame_id: result.next_frame_id ?? null,
          action_results: result.action_results,
        },
      };
    },
  });

  pi.registerTool({
    name: "computer_inspect",
    label: "Inspect a region of the screen",
    description:
      "Crop a region from the last frame and return it as an image content block the " +
      "model can see, plus the mapping to translate crop-relative coordinates back to " +
      "global desktop coordinates: global_origin is the crop's top-left in desktop " +
      "pixels; normalized_1000_origin is the crop's top-left in the original frame's " +
      "normalized_1000 space. To click something found in a crop, add the crop's " +
      "global_origin to the crop-relative point, or use normalized_1000_origin for " +
      "normalized_1000 coordinates. scale zooms the crop (1 = 100%). " +
      "The region is a structured object, not a JSON string.",
    promptSnippet: "Zoom into a region of the observed frame to read fine detail.",
    parameters: Type.Object({
      frame_id: Type.String({ description: "frame_id from the most recent computer_observe" }),
      region: Type.Object({
        x: Type.Number({ minimum: 0, description: "Region left in the frame's coordinate space" }),
        y: Type.Number({ minimum: 0, description: "Region top in the frame's coordinate space" }),
        width: Type.Number({ minimum: 1, description: "Region width" }),
        height: Type.Number({ minimum: 1, description: "Region height" }),
        coordinate_space: Type.Optional(coordinateSpace),
      }),
      scale: Type.Optional(Type.Number({ minimum: 1, maximum: 8, description: "Integer zoom applied to the crop" })),
    }),
    async execute(toolCallId, params, signal) {
      if (signal?.aborted) return cancelledResult();
      const c = await getClient();
      const sid = await ensureSession({ signal });
      const result = await c.inspect(
        {
          session_id: sid,
          frame_id: params.frame_id,
          region: {
            ...params.region,
            coordinate_space: (params.region.coordinate_space ?? "normalized_1000") as "normalized_1000" | "image_pixels",
          },
          scale: params.scale,
        },
        { signal },
      );
      const lines = [
        `frame_id: ${result.frame_id}`,
        `crop: ${result.width}x${result.height}`,
        `global_origin: ${result.mapping.global_origin.join(",")}`,
        `normalized_1000_origin: ${result.mapping.normalized_1000_origin.join(",")}`,
      ];
      return {
        content: [
          { type: "text", text: lines.join("\n") },
          { type: "image", data: result.image_base64, mimeType: result.image_mime_type },
        ],
        details: { width: result.width, height: result.height, mapping: result.mapping },
      };
    },
  });

  // --- Commands ------------------------------------------------------------

  function sessionCommand(
    action: "start" | "pause" | "resume" | "stop" | "takeover" | "release",
    verb: string,
  ) {
    return {
      description: `${verb} the computer-use session`,
      handler: async (_args: string, ctx: ExtensionContext) => {
        try {
          const c = await getClient();
          const s = await c.session(action, sessionId ? { session_id: sessionId } : {});
          sessionId = s.session_id;
          ctx.ui.notify(
            `${verb}: ${s.session_id} state=${s.state} paused=${s.paused} takeover=${s.user_takeover} lock=${s.lock_held}`,
            "info",
          );
        } catch (err) {
          ctx.ui.notify(`${verb} failed: ${toToolError(err).message}`, "error");
        }
      },
    };
  }

  pi.registerCommand("computer-status", {
    description: "Show computer-use daemon health and the current session state",
    handler: async (_args: string, ctx: ExtensionContext) => {
      try {
        const c = await getClient();
        const health = await c.health();
        const lines = [
          `daemon: v${health.version} ${health.ready ? "ready" : "NOT READY"}`,
          `permissions: screen_recording=${health.permissions.screen_recording} accessibility=${health.permissions.accessibility}`,
          `active_sessions: ${health.active_sessions}`,
        ];
        let sessionLine = "session: none";
        try {
          const s = await c.session("status", {});
          sessionId = s.session_id;
          sessionLine = `session: ${s.session_id} state=${s.state} paused=${s.paused} takeover=${s.user_takeover} lock=${s.lock_held}`;
          if (s.current_frame_id) sessionLine += ` frame=${s.current_frame_id}`;
          if (s.trace_dir) sessionLine += ` trace=${s.trace_dir}`;
        } catch (err) {
          if (!(err instanceof ComputerUseError && err.code === "SESSION_NOT_FOUND")) {
            sessionLine = `session: ${toToolError(err).message}`;
          }
        }
        ctx.ui.notify([...lines, sessionLine].join("\n"), "info");
      } catch (err) {
        ctx.ui.notify(`computer-use daemon: ${toToolError(err).message}`, "error");
      }
    },
  });

  pi.registerCommand("computer-start", sessionCommand("start", "started"));
  pi.registerCommand("computer-pause", sessionCommand("pause", "paused"));
  pi.registerCommand("computer-resume", sessionCommand("resume", "resumed"));
  pi.registerCommand("computer-stop", sessionCommand("stop", "stopped"));
  pi.registerCommand("computer-takeover", sessionCommand("takeover", "takeover"));
  pi.registerCommand("computer-release", sessionCommand("release", "released"));

  pi.registerCommand("computer-observe", {
    description: "Capture a screenshot of the desktop to a PNG file",
    handler: async (_args: string, ctx: ExtensionContext) => {
      try {
        const sid = await ensureSession();
        const c = await getClient();
        const frame = await c.observe({ session_id: sid, include_image: true });
        if (!frame.image_base64) throw new Error("daemon returned no image");
        const dest = join(ctx.cwd, `computer-use-${frame.frame_id}.png`);
        const { writeFileSync } = await import("node:fs");
        writeFileSync(dest, Buffer.from(frame.image_base64, "base64"));
        ctx.ui.notify(`screenshot saved: ${dest} (${frame.width}x${frame.height}, frame ${frame.frame_id})`, "info");
      } catch (err) {
        ctx.ui.notify(`observe failed: ${toToolError(err).message}`, "error");
      }
    },
  });

  // --- Lifecycle -----------------------------------------------------------
  // Pi fires session_shutdown before the extension runtime is torn down
  // (quit, /reload, session switch). Stop our session (releases the daemon's
  // control lock) and close the socket.

  pi.on("session_shutdown", async () => {
    if (!sessionId) return;
    const c = client;
    if (!c) return;
    try {
      await c.session("stop", { session_id: sessionId });
    } catch {
      // The daemon may already be gone; nothing to clean up.
    } finally {
      sessionId = null;
      c.close();
      client = null;
    }
  });
}
