// OpenCode plugin for the computer-use runtime.
//
// Target API: OpenCode (https://opencode.ai) >= 1.x plugin API. A plugin is a
// TypeScript module whose default export is a function receiving
// `(config, context)` and returning `{ tools }`, where each tool has a
// description, a zod input schema, and an async `run` returning
// `string | { content: ... } | object`.
//
// The minimal API types are declared locally (instead of importing the
// `opencode` package) so this package builds standalone; the shapes match
// OpenCode's documented plugin contract.
//
// Usage: add to opencode config
//   {
//     "plugin": ["./node_modules/@computer-use/opencode-adapter/dist/index.js"]
//   }
// or copy config/opencode.config.json as a starting point. The daemon socket
// can be overridden with COMPUTER_USE_SOCKET.

import {
  connect,
  ComputerUseError,
  type ComputerAction,
  type ComputerUseClient,
} from "@computer-use/sdk";

// ---------------------------------------------------------------------------
// Minimal OpenCode plugin API types (match opencode >= 1.x).
// ---------------------------------------------------------------------------

export interface OpenCodeToolArgs {
  [key: string]: unknown;
}

export interface OpenCodeTool {
  description: string;
  args: Record<string, unknown>; // zod schema object
  run: (input: OpenCodeToolArgs, context: unknown) => Promise<unknown>;
}

export interface OpenCodePluginResult {
  tools: Record<string, OpenCodeTool>;
}

export interface OpenCodeConfig {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  [key: string]: any;
}

export type OpenCodePlugin = (
  config: OpenCodeConfig,
  context: unknown,
) => OpenCodePluginResult | Promise<OpenCodePluginResult>;

// ---------------------------------------------------------------------------
// Tool registration
// ---------------------------------------------------------------------------

/** JSON-schema-ish description of `actions` used in error messages. */
const ACTIONS_HINT =
  'JSON array of actions, e.g. [{"type":"click","x":500,"y":400,"button":"left","coordinate_space":"normalized_1000"}]. Types: click, double_click, move, type, key, scroll, drag, wait. Coordinates default to normalized_1000.';

export interface ComputerUseOpenCodeOptions {
  /** Override the daemon socket path (default: ~/.computer-use/runtime.sock). */
  socketPath?: string;
}

/** Build the OpenCode plugin. */
export function computerUsePlugin(
  options: ComputerUseOpenCodeOptions = {},
): OpenCodePlugin {
  return async function computerUseOpenCodePlugin(
    _config: OpenCodeConfig,
  ): Promise<OpenCodePluginResult> {
    let client: ComputerUseClient | undefined;

    async function getClient(): Promise<ComputerUseClient> {
      if (client) return client;
      client = await connect({
        socketPath: options.socketPath ?? process.env.COMPUTER_USE_SOCKET,
      });
      return client;
    }

    function asError(err: unknown): string {
      if (err instanceof ComputerUseError) {
        return `ERROR ${err.code}: ${err.message}`;
      }
      return `ERROR: ${(err as Error).message ?? String(err)}`;
    }

    const tools: Record<string, OpenCodeTool> = {
      computer_observe: {
        description:
          "Capture the current desktop. Returns the frame_id, dimensions, active application, and the path of the stored screenshot. Use frame_id for computer_act.",
        args: {
          session_id: { type: "string", optional: true },
          include_cursor: { type: "boolean", optional: true },
          max_width: { type: "number", optional: true, description: "downscale width" },
          include_image: { type: "boolean", optional: true, default: false },
        },
        run: async (input: OpenCodeToolArgs) => {
          try {
            const c = await getClient();
            const frame = await c.observe({
              session_id: typeof input.session_id === "string" ? input.session_id : undefined,
              include_cursor: input.include_cursor as boolean | undefined,
              max_width: input.max_width as number | undefined,
              include_image: input.include_image as boolean | undefined,
            });
            return {
              frame_id: frame.frame_id,
              session_id: frame.session_id,
              width: frame.width,
              height: frame.height,
              display_id: frame.display_id,
              scale_factor: frame.scale_factor,
              active_application: frame.active_application ?? null,
              image_path: frame.image_path,
              captured_at: frame.captured_at,
            };
          } catch (err) {
            return asError(err);
          }
        },
      },

      computer_act: {
        description:
          `Execute a batch of actions against the frame described by frame_id. ${ACTIONS_HINT} Acting on a stale frame is rejected (STALE_FRAME) — re-observe first.`,
        args: {
          session_id: { type: "string" },
          frame_id: { type: "string", description: "frame_id from the most recent computer_observe" },
          actions: { type: "string", description: ACTIONS_HINT },
          wait_policy: { type: "string", optional: true, enum: ["none", "fixed", "until_stable"] },
          fixed_wait_ms: { type: "number", optional: true },
          return_screenshot: { type: "boolean", optional: true, default: false },
        },
        run: async (input: OpenCodeToolArgs) => {
          try {
            let actions: ComputerAction[];
            try {
              actions = JSON.parse(String(input.actions)) as ComputerAction[];
            } catch {
              return `ERROR INVALID_PARAMS: actions is not valid JSON. ${ACTIONS_HINT}`;
            }
            const c = await getClient();
            const result = await c.act({
              session_id: String(input.session_id),
              frame_id: String(input.frame_id),
              actions,
              wait_policy: input.wait_policy as "none" | "fixed" | "until_stable" | undefined,
              fixed_wait_ms: input.fixed_wait_ms as number | undefined,
              return_screenshot: input.return_screenshot as boolean | undefined,
            });
            return {
              executed: result.executed,
              screen_changed: result.screen_changed,
              stable: result.stable,
              next_frame_id: result.next_frame_id ?? null,
              action_results: result.action_results,
            };
          } catch (err) {
            return asError(err);
          }
        },
      },

      computer_inspect: {
        description:
          "Crop a region from the last frame. Returns the crop as a PNG file path plus the mapping to translate crop coordinates back to global desktop coordinates.",
        args: {
          session_id: { type: "string" },
          frame_id: { type: "string" },
          x: { type: "number" },
          y: { type: "number" },
          width: { type: "number" },
          height: { type: "number" },
          coordinate_space: { type: "string", optional: true, enum: ["normalized_1000", "image_pixels"] },
          scale: { type: "number", optional: true },
        },
        run: async (input: OpenCodeToolArgs) => {
          try {
            const c = await getClient();
            const result = await c.inspect({
              session_id: String(input.session_id),
              frame_id: String(input.frame_id),
              region: {
                x: Number(input.x),
                y: Number(input.y),
                width: Number(input.width),
                height: Number(input.height),
                coordinate_space: (input.coordinate_space as "normalized_1000" | "image_pixels" | undefined) ?? "normalized_1000",
              },
              scale: input.scale as number | undefined,
            });
            return {
              frame_id: result.frame_id,
              width: result.width,
              height: result.height,
              mapping: result.mapping,
              image_base64_length: result.image_base64.length,
            };
          } catch (err) {
            return asError(err);
          }
        },
      },

      computer_session: {
        description:
          "Manage the computer-use session: start, status, pause, resume, stop, takeover, release. Sessions gate access to keyboard and mouse.",
        args: {
          action: { type: "string", enum: ["start", "status", "pause", "resume", "stop", "takeover", "release"] },
          session_id: { type: "string", optional: true },
          display_id: { type: "string", optional: true },
        },
        run: async (input: OpenCodeToolArgs) => {
          try {
            const c = await getClient();
            const result = await c.session(input.action as "start", {
              session_id: input.session_id as string | undefined,
              display_id: input.display_id as string | undefined,
            });
            return {
              session_id: result.session_id,
              state: result.state,
              paused: result.paused,
              user_takeover: result.user_takeover,
              lock_held: result.lock_held,
              started_by: result.started_by,
              ...(result.current_frame_id ? { current_frame_id: result.current_frame_id } : {}),
            };
          } catch (err) {
            return asError(err);
          }
        },
      },
    };

    return { tools };
  };
}

/** Default export expected by OpenCode's plugin loader. */
export default computerUsePlugin();
