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

import { unlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
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
} from "@computer-use/sdk";

// Wire types generated from the Rust protocol source of truth (never hand-edited).
export * from "./generated/protocol.js";

// ---------------------------------------------------------------------------
// Daemon connection + session state (module-scoped per Pi runtime instance)
// ---------------------------------------------------------------------------

let client: ComputerUseClient | null = null;
let sessionId: string | null = null;
/** True when we started the current session — only then may we stop it. */
let ownsSession = false;

/**
 * Identity this extension reports when it starts a session. The daemon
 * records it on the session; the owner is the only client allowed to stop it.
 */
const PI_CLIENT_INFO = {
  client_id: "pi-extension",
  client_name: "Pi",
  client_instance_id: `pi-${process.pid}-${Math.random().toString(36).slice(2, 8)}`,
};

async function getClient(): Promise<ComputerUseClient> {
  if (!client) {
    client = await connect({
      socketPath: process.env.COMPUTER_USE_SOCKET,
      clientInfo: PI_CLIENT_INFO,
    });
  }
  return client;
}

/**
 * What to do when a session owned by *another* client is already active.
 * Only `reject` (the default) exists: the start attempt is refused and the
 * daemon's CONTROL_LOCKED error (with the owner's identity, never a token)
 * surfaces.
 *
 * The pre-0.3 observe-only policies were removed: a read-only attach without
 * the foreign session's observation token grants nothing (the daemon refuses
 * every sensitive read without a capability), so it was silently powerless
 * at best and misleading at worst. Setting
 * `COMPUTER_USE_EXISTING_SESSION_POLICY=read_only` (or the older `attach`)
 * still configures the *daemon* to refuse — the value is honored with a
 * deprecation warning and behaves exactly like `reject`. `attach_with_token`
 * is not offered either — Pi has no way to obtain another client's token.
 */
type ExistingSessionPolicy = "reject";

function existingSessionPolicy(): ExistingSessionPolicy {
  const v = process.env.COMPUTER_USE_EXISTING_SESSION_POLICY;
  if (v === "read_only") {
    // Removed in 0.3: observe-only attachment without the foreign session's
    // observation token was refused by the daemon anyway. Configs that set it
    // now get the same `reject` behavior, loudly.
    console.warn(
      "COMPUTER_USE_EXISTING_SESSION_POLICY=read_only is deprecated and removed; treating it as reject (the default). A read-only attach without the session's observation token grants nothing.",
    );
  } else if (v === "attach") {
    // `attach` was the pre-0.2 name for the removed read-only behavior.
    console.warn(
      "COMPUTER_USE_EXISTING_SESSION_POLICY=attach is deprecated and removed; treating it as reject (the default).",
    );
  }
  return "reject";
}

/**
 * Resolve the current session, starting one if none exists. Ownership and the
 * control token live in the SDK's SessionCredential: a session this extension
 * starts is held (token issued once at start), and `ownsSession` mirrors that
 * — an extension without the credential never claims to own anything.
 */
async function ensureSession(options?: RequestOptions): Promise<string> {
  if (sessionId) {
    // Cached — but re-confirm the credential matches (it is dropped if the
    // daemon says the session is gone, e.g. another owner stopped it).
    const cred = client?.getSessionCredential();
    if (!cred || cred.sessionId !== sessionId) sessionId = null;
    else return sessionId;
  }
  const c = await getClient();
  const s = await c.ensureSession(undefined, options, PI_CLIENT_INFO, existingSessionPolicy());
  sessionId = s.session_id;
  ownsSession = c.getSessionCredential()?.sessionId === s.session_id;
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
// Screenshot storage for the /computer-observe command
// ---------------------------------------------------------------------------

/** Map an image MIME type to a file extension. Throws for unknown MIME types. */
export function extensionForMime(mimeType: string): string {
  if (mimeType === "image/png") return "png";
  if (mimeType === "image/jpeg") return "jpg";
  throw new Error(`unsupported image MIME type: ${mimeType}`);
}

/**
 * Temporary screenshots written by the /computer-observe command. Files go to
 * the system temp directory (never the project/cwd), are readable only by the
 * current user, and are removed on session shutdown or when they age out.
 * Traces reference the daemon's own frames directory, not these copies, so
 * cleaning them up never breaks a trace.
 */
class TempImageStore {
  private files: { path: string; createdMs: number }[] = [];

  /** Write a screenshot and track it. Returns the destination path. */
  save(sessionId: string, frameId: string, imageBase64: string, mimeType: string): string {
    const extension = extensionForMime(mimeType);
    const dest = join(tmpdir(), `oc-computer-use-${sessionId}-${frameId}.${extension}`);
    writeFileSync(dest, Buffer.from(imageBase64, "base64"), { mode: 0o600 });
    this.files.push({ path: dest, createdMs: Date.now() });
    this.prune(60 * 60 * 1000); // age out screenshots older than 1h
    return dest;
  }

  /** Remove tracked screenshots older than `maxAgeMs`. */
  prune(maxAgeMs: number): void {
    const now = Date.now();
    this.files = this.files.filter((f) => {
      if (now - f.createdMs > maxAgeMs) {
        try {
          unlinkSync(f.path);
        } catch {
          // already gone
        }
        return false;
      }
      return true;
    });
  }

  /** Remove every tracked screenshot (session shutdown / extension exit). */
  cleanup(): void {
    for (const f of this.files) {
      try {
        unlinkSync(f.path);
      } catch {
        // already gone
      }
    }
    this.files = [];
  }
}

const imageStore = new TempImageStore();

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
  text: Type.Optional(Type.String({ maxLength: 4096, description: "Text to type (max 4096 chars; logged redacted by default)" })),
  method: Type.Optional(StringEnum(["keyboard", "clipboard"], { description: "Text input method (default keyboard)" })),
  keys: Type.Optional(Type.Array(Type.String(), { minItems: 1, maxItems: 8, description: "Key names (max 8), e.g. [\"return\"] or [\"cmd\", \"c\"]" })),
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
      "Only the client that started a session owns it: knowing a session id grants no " +
      "control, and `stop` is honored only for the owner (the daemon verifies a token " +
      "issued once at start). " +
      "Omit session_id to act on the current session. " +
      "Output: session id, state, paused, user_takeover, lock_held, started_by.",
    promptSnippet: "Start or inspect a computer-use session, or manage pause/takeover state.",
    parameters: Type.Object({
      action: StringEnum(["start", "status", "pause", "resume", "stop", "takeover", "release"], {
        description: "Session action",
      }),
      session_id: Type.Optional(Type.String({ description: "Omit to act on the current session" })),
      display_id: Type.Optional(Type.String({ description: "Display to capture (defaults to primary)" })),
      // Round 9 / P0-5: session isolation configuration.
      target: Type.Optional(
        Type.Object(
          {
            bundle_id: Type.Optional(Type.String({ description: "App bundle id, e.g. com.google.Chrome" })),
            pid: Type.Optional(Type.Number({ description: "App process id" })),
            window_id: Type.Optional(Type.Number({ description: "Window id (CGWindowNumber)" })),
          },
          { description: "Scope the session to one app/window" },
        ),
      ),
      pointer_policy: Type.Optional(
        StringEnum(["isolated_only", "isolated_preferred", "physical_allowed"], {
          description:
            "isolated_only never touches the real cursor; isolated_preferred prefers isolation " +
            "(physical only when explicitly allowed); physical_allowed may borrow the cursor",
        }),
      ),
      focus_policy: Type.Optional(
        StringEnum(["strict", "activate_target"], {
          description:
            "strict rejects type/key when focus is not on the target (never steals foreground); " +
            "activate_target is experimental/unsupported",
        }),
      ),
    }),
    async execute(toolCallId, params, signal) {
      if (signal?.aborted) return cancelledResult();
      const c = await getClient();
      const result = await c.session(
        params.action as SessionAction,
        {
          session_id: params.session_id ?? undefined,
          display_id: params.display_id ?? undefined,
          ...(params.target !== undefined
            ? { target: params.target }
            : {}),
          ...(params.pointer_policy !== undefined
            ? { pointer_policy: params.pointer_policy }
            : {}),
          ...(params.focus_policy !== undefined
            ? { focus_policy: params.focus_policy }
            : {}),
          // Record our identity when we create the session (ownership).
          ...(params.action === "start" ? { ...PI_CLIENT_INFO } : {}),
        },
        { signal },
      );
      sessionId = result.session_id;
      if (params.action === "stop") sessionId = null;
      // Ownership is whatever token we hold: the SDK stores the token issued
      // by start (and drops it on stop), so this mirrors the daemon exactly.
      ownsSession = c.getSessionCredential()?.sessionId === result.session_id;
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
          const s = await c.session(action, {
            ...(sessionId ? { session_id: sessionId } : {}),
            // Record our identity when we create the session (ownership).
            ...(action === "start" ? { ...PI_CLIENT_INFO } : {}),
          });
          sessionId = s.session_id;
          if (action === "stop") sessionId = null;
          ownsSession = c.getSessionCredential()?.sessionId === s.session_id;
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
          // v3: `status` is a sensitive read — without the observation
          // credential (a foreign session) the daemon refuses it. The coarse
          // `session.summary` is the public tokenless view, good enough for a
          // status line. Read-only: never adopt a foreign session here (that
          // would bypass the existing-session policy for later calls).
          const s = await c.sessionSummary();
          if (s.session_id) {
            sessionLine = `session: ${s.session_id} state=${s.state ?? "?"} lock=${s.lock_held}`;
            if (s.owner_client_name) sessionLine += ` owner=${s.owner_client_name}`;
            if (s.owner_client_id && s.owner_client_id !== PI_CLIENT_INFO.client_id) {
              sessionLine += ` (client ${s.owner_client_id})`;
            }
            if (s.message) sessionLine += ` — ${s.message}`;
          }
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
    description: "Capture a screenshot of the desktop to a temporary file",
    handler: async (_args: string, ctx: ExtensionContext) => {
      try {
        const sid = await ensureSession();
        const c = await getClient();
        const frame = await c.observe({ session_id: sid, include_image: true });
        if (!frame.image_base64) throw new Error("daemon returned no image");
        // Temp dir + MIME-derived extension, tracked for cleanup on shutdown.
        const dest = imageStore.save(sid, frame.frame_id, frame.image_base64, frame.image_mime_type);
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
    const c = client;
    client = null;
    try {
      // Only stop a session we created. A session another client owns (attach
      // policy) keeps running for its owner — we just let go of it.
      if (c && ownsSession && sessionId) {
        await c.session("stop", { session_id: sessionId });
      }
    } catch {
      // The daemon may already be gone; nothing to clean up.
    } finally {
      sessionId = null;
      ownsSession = false;
      imageStore.cleanup();
      c?.close();
    }
  });
}
