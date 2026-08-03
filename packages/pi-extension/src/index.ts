// Driver-mode extension for the computer-use runtime.
//
// This is the model-agnostic harness described in the architecture: the
// *driver mode* loop — observe → model decides → runtime executes — with the
// runtime in the daemon and the model anywhere (Pi, an API, a local model,
// a test stub). The core runtime contains no model SDK; this package is the
// thin driver layer that connects an agent to the four tools:
//
//   computer_observe  computer_act  computer_inspect  computer_session
//
// `registerTools()` returns schema descriptions for hosts that introspect
// tools; `handleTool()` executes a named tool; `runDriverLoop()` drives the
// full observe→decide→act cycle against any `DriverModel` implementation.

import {
  connect,
  ComputerUseError,
  type ComputerAction,
  type ComputerUseClient,
  type ObserveResult,
  type SessionResult,
  TransportError,
} from "@computer-use/sdk";

// ---------------------------------------------------------------------------
// Tool schema (host-agnostic)
// ---------------------------------------------------------------------------

export interface ToolInputField {
  type: "string" | "number" | "boolean";
  optional?: boolean;
  description?: string;
  enum?: string[];
}

export interface ToolSchema {
  name: string;
  description: string;
  input: Record<string, ToolInputField>;
}

export type ToolResult =
  | { ok: true; data: unknown }
  | { ok: false; error: string; code?: string };

// ---------------------------------------------------------------------------
// Driver loop model interface
// ---------------------------------------------------------------------------

export interface ActionRecord {
  step: number;
  frame_id: string;
  actions: ComputerAction[];
  result: {
    executed: boolean;
    screen_changed: boolean;
    stable: boolean;
    reports: { index: number; status: string; duration_ms: number; error?: string }[];
  };
}

export type ModelDecision =
  | { kind: "act"; actions: ComputerAction[] }
  | { kind: "done"; summary?: string }
  | { kind: "error"; message: string };

/** Anything that can decide the next action from a frame + history. */
export interface DriverModel {
  decide(context: {
    system: string;
    frame: ObserveResult;
    history: ActionRecord[];
    step: number;
  }): Promise<ModelDecision>;
}

export const DRIVER_SYSTEM_PROMPT = `You are controlling a macOS computer through screenshots.
Every loop iteration you receive a frame_id and the screenshot location. Return ONE decision:
- {"kind":"act","actions":[...]} — a JSON array of actions on the frame:
  click/double_click {x,y,button,coordinate_space}, move {x,y}, type {text},
  key {keys}, scroll {delta_x,delta_y}, drag {from,to}, wait {duration_ms}.
  Coordinates are normalized_1000 (image treated as 1000x1000, top-left origin).
- {"kind":"done","summary":"..."} — when the task is complete.
- {"kind":"error","message":"..."} — when the task cannot continue.
Acting on a stale frame is rejected (STALE_FRAME); re-observe and retry.`;

// ---------------------------------------------------------------------------
// The extension
// ---------------------------------------------------------------------------

export interface PiExtensionOptions {
  socketPath?: string;
  /** System prompt override for runDriverLoop. */
  systemPrompt?: string;
}

export class ComputerUseExtension {
  readonly client: ComputerUseClient;
  private session: SessionResult | null = null;
  private lastFrame: ObserveResult | null = null;

  constructor(client: ComputerUseClient) {
    this.client = client;
  }

  static async create(options: PiExtensionOptions = {}): Promise<ComputerUseExtension> {
    const client = await connect({
      socketPath: options.socketPath ?? process.env.COMPUTER_USE_SOCKET,
    });
    return new ComputerUseExtension(client);
  }

  /** The four tool schemas, for hosts that introspect tool lists. */
  toolSchemas(): ToolSchema[] {
    return [
      {
        name: "computer_observe",
        description:
          "Capture the current desktop. Returns frame_id, size, active application and the stored screenshot path. The frame_id is the reference for computer_act.",
        input: {
          include_cursor: { type: "boolean", optional: true, description: "draw the cursor into the image" },
          max_width: { type: "number", optional: true, description: "downscale so width does not exceed this" },
        },
      },
      {
        name: "computer_act",
        description:
          "Execute a batch of actions on the frame referenced by frame_id (must be the most recent observe; stale frames are rejected with STALE_FRAME).",
        input: {
          frame_id: { type: "string", description: "from the most recent computer_observe" },
          actions: { type: "string", description: "JSON array of action objects (see driver prompt)" },
          wait_policy: { type: "string", optional: true, enum: ["none", "fixed", "until_stable"] },
          fixed_wait_ms: { type: "number", optional: true },
        },
      },
      {
        name: "computer_inspect",
        description:
          "Crop a region of the last frame. Returns the crop plus a mapping to translate crop coordinates back to global desktop coordinates.",
        input: {
          frame_id: { type: "string" },
          x: { type: "number" },
          y: { type: "number" },
          width: { type: "number" },
          height: { type: "number" },
          coordinate_space: { type: "string", optional: true, enum: ["normalized_1000", "image_pixels"] },
        },
      },
      {
        name: "computer_session",
        description:
          "Session lifecycle: start, status, pause, resume, stop, takeover, release. Sessions gate keyboard/mouse access.",
        input: {
          action: { type: "string", enum: ["start", "status", "pause", "resume", "stop", "takeover", "release"] },
        },
      },
    ];
  }

  /** Ensure an active session exists and remember it. */
  async ensureSession(): Promise<SessionResult> {
    if (this.session && (this.session.state === "active" || this.session.state === "paused")) {
      return this.session;
    }
    this.session = await this.client.ensureSession();
    return this.session;
  }

  /** Execute one named tool. `args` is a plain object of tool arguments. */
  async handleTool(name: string, args: Record<string, unknown> = {}): Promise<ToolResult> {
    try {
      switch (name) {
        case "computer_observe": {
          const session = await this.ensureSession();
          const frame = await this.client.observe({
            session_id: session.session_id,
            include_cursor: args.include_cursor as boolean | undefined,
            max_width: args.max_width as number | undefined,
          });
          this.lastFrame = frame;
          return {
            ok: true,
            data: {
              frame_id: frame.frame_id,
              session_id: frame.session_id,
              width: frame.width,
              height: frame.height,
              display_id: frame.display_id,
              scale_factor: frame.scale_factor,
              active_application: frame.active_application ?? null,
              active_window: frame.active_window ?? null,
              image_path: frame.image_path,
              captured_at: frame.captured_at,
            },
          };
        }
        case "computer_act": {
          const session = await this.ensureSession();
          let actions: ComputerAction[];
          try {
            actions = JSON.parse(String(args.actions)) as ComputerAction[];
          } catch {
            return { ok: false, error: "actions is not valid JSON", code: "INVALID_PARAMS" };
          }
          const result = await this.client.act({
            session_id: session.session_id,
            frame_id: String(args.frame_id),
            actions,
            wait_policy: args.wait_policy as "none" | "fixed" | "until_stable" | undefined,
            fixed_wait_ms: args.fixed_wait_ms as number | undefined,
          });
          return {
            ok: true,
            data: {
              executed: result.executed,
              screen_changed: result.screen_changed,
              stable: result.stable,
              next_frame_id: result.next_frame_id ?? null,
              action_results: result.action_results,
            },
          };
        }
        case "computer_inspect": {
          const session = await this.ensureSession();
          const result = await this.client.inspect({
            session_id: session.session_id,
            frame_id: String(args.frame_id),
            region: {
              x: Number(args.x),
              y: Number(args.y),
              width: Number(args.width),
              height: Number(args.height),
              coordinate_space: (args.coordinate_space as "normalized_1000" | "image_pixels" | undefined) ?? "normalized_1000",
            },
            scale: args.scale as number | undefined,
          });
          return {
            ok: true,
            data: {
              frame_id: result.frame_id,
              width: result.width,
              height: result.height,
              mapping: result.mapping,
              image_base64_length: result.image_base64.length,
            },
          };
        }
        case "computer_session": {
          const result = await this.client.session(
            args.action as "start",
            { session_id: typeof args.session_id === "string" ? args.session_id : undefined },
          );
          this.session = result;
          return {
            ok: true,
            data: {
              session_id: result.session_id,
              state: result.state,
              paused: result.paused,
              user_takeover: result.user_takeover,
              lock_held: result.lock_held,
              started_by: result.started_by,
              ...(result.current_frame_id ? { current_frame_id: result.current_frame_id } : {}),
            },
          };
        }
        default:
          return { ok: false, error: `unknown tool \`${name}\``, code: "METHOD_NOT_FOUND" };
      }
    } catch (err) {
      if (err instanceof ComputerUseError) {
        return { ok: false, error: err.message, code: err.code };
      }
      if (err instanceof TransportError) {
        return { ok: false, error: err.message, code: "TRANSPORT" };
      }
      return { ok: false, error: (err as Error).message ?? String(err) };
    }
  }

  /**
   * The driver-mode loop: observe → model decides → runtime executes →
   * repeat, until the model says done, errors, or maxSteps is reached.
   * STALE_FRAME rejections are retried with a fresh observe automatically.
   */
  async runDriverLoop(
    model: DriverModel,
    options: { maxSteps?: number; systemPrompt?: string } = {},
  ): Promise<{ completed: boolean; steps: number; summary?: string; reason: string }> {
    const maxSteps = options.maxSteps ?? 10;
    const system = options.systemPrompt ?? DRIVER_SYSTEM_PROMPT;
    const history: ActionRecord[] = [];

    for (let step = 1; step <= maxSteps; step++) {
      const session = await this.ensureSession();
      const frame = await this.client.observe({ session_id: session.session_id });
      this.lastFrame = frame;

      const decision = await model.decide({ system, frame, history, step });

      if (decision.kind === "done") {
        return { completed: true, steps: step, summary: decision.summary, reason: "done" };
      }
      if (decision.kind === "error") {
        return { completed: false, steps: step, reason: `model error: ${decision.message}` };
      }

      try {
        const result = await this.client.act({
          session_id: session.session_id,
          frame_id: frame.frame_id,
          actions: decision.actions,
        });
        history.push({
          step,
          frame_id: frame.frame_id,
          actions: decision.actions,
          result: {
            executed: result.executed,
            screen_changed: result.screen_changed,
            stable: result.stable,
            reports: result.action_results,
          },
        });
      } catch (err) {
        if (err instanceof ComputerUseError && err.code === "STALE_FRAME") {
          // The screen changed between observe and act; loop re-observes.
          continue;
        }
        throw err;
      }
    }
    return { completed: false, steps: maxSteps, reason: `max steps (${maxSteps}) reached` };
  }

  close(): void {
    this.client.close();
  }
}
