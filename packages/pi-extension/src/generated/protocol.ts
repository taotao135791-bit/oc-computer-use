/* eslint-disable */
/**
 * GENERATED FILE — do not edit by hand.
 * Source of truth: the Rust wire types (crates/cu-core/src/protocol.rs etc.).
 * Regenerate with `pnpm generate:protocol`; `pnpm check:protocol` fails on drift.
 */
/**
 * A single atomic computer action.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "ComputerAction".
 */
export type ComputerAction =
  | {
      button: MouseButton;
      coordinate_space: CoordinateSpace;
      type: "click";
      x: number;
      y: number;
      [k: string]: unknown;
    }
  | {
      button: MouseButton;
      coordinate_space: CoordinateSpace;
      type: "double_click";
      x: number;
      y: number;
      [k: string]: unknown;
    }
  | {
      coordinate_space: CoordinateSpace;
      duration_ms?: number | null;
      type: "move";
      x: number;
      y: number;
      [k: string]: unknown;
    }
  | {
      /**
       * How text is inserted. `keyboard` synthesizes key events with the unicode string; `clipboard` swaps the pasteboard, pastes, and restores it (with a fallback when synthetic key events are unreliable for CJK input).
       */
      method?: "keyboard" | "clipboard";
      text: string;
      type: "type";
      [k: string]: unknown;
    }
  | {
      keys: string[];
      type: "key";
      [k: string]: unknown;
    }
  | {
      coordinate_space: CoordinateSpace;
      delta_x?: number;
      delta_y?: number;
      type: "scroll";
      x?: number | null;
      y?: number | null;
      [k: string]: unknown;
    }
  | {
      coordinate_space: CoordinateSpace;
      duration_ms?: number | null;
      from: Point;
      to: Point;
      type: "drag";
      [k: string]: unknown;
    }
  | {
      duration_ms: number;
      type: "wait";
      [k: string]: unknown;
    };
/**
 * Mouse buttons understood by the macOS driver.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "MouseButton".
 */
export type MouseButton = "left" | "right" | "middle";
/**
 * Coordinate space a caller used when describing a location or region.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "CoordinateSpace".
 */
export type CoordinateSpace = "normalized_1000" | "image_pixels";
/**
 * What the runtime should do after executing an action batch.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "WaitPolicy".
 */
export type WaitPolicy = "none" | "fixed" | "until_stable";
/**
 * Shared token fields for **cross-session** sensitive reads: `runtime.pointer`, `runtime.active_application`, `runtime.desktop_layout`, `trace.list`, `trace.summaries`. Unlike the session-addressed reads these methods have no `session_id`, so any valid observation or control token is accepted — the token proves the caller is a trusted client of this daemon. No token → `OBSERVATION_TOKEN_REQUIRED`; a token matching nothing → `INVALID_OBSERVATION_TOKEN`.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "CapabilityTokenParams".
 */
export type CapabilityTokenParams =
  | {
      control_token?: string;
      observation_token: string;
      [k: string]: unknown;
    }
  | {
      control_token: string;
      observation_token?: string;
      [k: string]: unknown;
    };
/**
 * Well-known structured error codes. These are the stable strings that appear in the `data.code` field of a JSON-RPC error response.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "ErrorCode".
 */
export type ErrorCode =
  | (
      | "PARSE_ERROR"
      | "INVALID_REQUEST"
      | "INVALID_PARAMS"
      | "METHOD_NOT_FOUND"
      | "INTERNAL"
      | "NOT_READY"
      | "PERMISSION"
      | "STALE_FRAME"
      | "CONTROL_LOCKED"
      | "PAUSED"
      | "USER_TAKEOVER"
      | "OUT_OF_BOUNDS"
      | "UNKNOWN_FRAME"
      | "SESSION_NOT_FOUND"
      | "INVALID_SESSION_STATE"
      | "CONFIRMATION_REQUIRED"
      | "CANCELLED"
      | "TRACE_ERROR"
      | "DRIVER_ERROR"
      | "UNSUPPORTED"
    )
  | "USER_TAKEOVER_ACTIVE"
  | "ACTION_TIMEOUT"
  | "CAPTURE_FAILED"
  | "CONTROL_TOKEN_REQUIRED"
  | "INVALID_CONTROL_TOKEN"
  | "OBSERVATION_TOKEN_REQUIRED"
  | "INVALID_OBSERVATION_TOKEN"
  | "SESSION_STOPPED"
  | "REQUEST_TIMEOUT"
  | "PROTOCOL_VERSION_MISMATCH"
  | "DAEMON_ADMIN_TOKEN_REQUIRED"
  | "INVALID_DAEMON_ADMIN_TOKEN"
  | "DAEMON_SHUTTING_DOWN";
/**
 * `computer.inspect` request.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "InspectParams".
 */
export type InspectParams =
  | {
      control_token?: string;
      frame_id?: string;
      /**
       * Observation (or control) token — required; a session id alone grants no observation permission.
       */
      observation_token: string;
      region?: Region1;
      scale?: number;
      session_id?: string;
      [k: string]: unknown;
    }
  | {
      control_token: string;
      frame_id?: string;
      /**
       * Observation (or control) token — required; a session id alone grants no observation permission.
       */
      observation_token?: string;
      region?: Region1;
      scale?: number;
      session_id?: string;
      [k: string]: unknown;
    };
/**
 * `computer.observe` request.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "ObserveParams".
 */
export type ObserveParams =
  | {
      /**
       * The session's control token — accepted in place of the observation token (control includes observation).
       */
      control_token?: string;
      display_id?: string;
      image_format?: string;
      include_cursor?: boolean;
      /**
       * When true, the response carries the image as base64. Off by default so a plain `observe` stays cheap; adapters that need pixels (MCP, vision harnesses) turn it on.
       */
      include_image?: boolean;
      jpeg_quality?: number;
      max_width?: number;
      /**
       * The session's **observation token**. Required — observing captures the desktop; a session id alone grants no observation permission. A valid control token is accepted in its place.
       */
      observation_token: string;
      session_id?: string;
      target?: string;
      [k: string]: unknown;
    }
  | {
      /**
       * The session's control token — accepted in place of the observation token (control includes observation).
       */
      control_token: string;
      display_id?: string;
      image_format?: string;
      include_cursor?: boolean;
      /**
       * When true, the response carries the image as base64. Off by default so a plain `observe` stays cheap; adapters that need pixels (MCP, vision harnesses) turn it on.
       */
      include_image?: boolean;
      jpeg_quality?: number;
      max_width?: number;
      /**
       * The session's **observation token**. Required — observing captures the desktop; a session id alone grants no observation permission. A valid control token is accepted in its place.
       */
      observation_token?: string;
      session_id?: string;
      target?: string;
      [k: string]: unknown;
    };
/**
 * Which macOS permission is missing. Used to generate actionable guidance.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "PermissionKind".
 */
export type PermissionKind = "screen_recording" | "accessibility";
/**
 * Actions a caller may issue against a session via `computer.session`.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "SessionAction".
 */
export type SessionAction = "start" | "status" | "pause" | "resume" | "stop" | "takeover" | "release";
/**
 * `computer.session` request.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "SessionParams".
 */
export type SessionParams =
  | {
      action?: "start";
      [k: string]: unknown;
    }
  | (
      | {
          action?: SessionAction;
          /**
           * Identity of the client performing the action; recorded on `start`.
           */
          client_id?: string;
          client_instance_id?: string;
          client_name?: string;
          /**
           * The session's control token. Required for `pause`/`resume`/`takeover`/ `release`/`stop`; `start` does not need one. `status` needs either this or the observation token (full status is a sensitive read).
           */
          control_token?: string;
          display_id?: string;
          /**
           * The session's observation token — accepted for `status` in place of the control token.
           */
          observation_token: string;
          session_id?: string;
          [k: string]: unknown;
        }
      | {
          action?: SessionAction;
          /**
           * Identity of the client performing the action; recorded on `start`.
           */
          client_id?: string;
          client_instance_id?: string;
          client_name?: string;
          /**
           * The session's control token. Required for `pause`/`resume`/`takeover`/ `release`/`stop`; `start` does not need one. `status` needs either this or the observation token (full status is a sensitive read).
           */
          control_token: string;
          display_id?: string;
          /**
           * The session's observation token — accepted for `status` in place of the control token.
           */
          observation_token?: string;
          session_id?: string;
          [k: string]: unknown;
        }
    )
  | {
      action?: SessionAction;
      /**
       * Identity of the client performing the action; recorded on `start`.
       */
      client_id?: string;
      client_instance_id?: string;
      client_name?: string;
      /**
       * The session's control token. Required for `pause`/`resume`/`takeover`/ `release`/`stop`; `start` does not need one. `status` needs either this or the observation token (full status is a sensitive read).
       */
      control_token: string;
      display_id?: string;
      /**
       * The session's observation token — accepted for `status` in place of the control token.
       */
      observation_token?: string;
      session_id?: string;
      [k: string]: unknown;
    };
/**
 * Lifecycle state of a computer-use session.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "SessionState".
 */
export type SessionState = "starting" | "active" | "paused" | "user_takeover" | "stopping" | "stopped" | "failed";
/**
 * How strictly a referenced `frame_id` must match the current screen.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "StaleFramePolicy".
 */
export type StaleFramePolicy = "Strict" | "VisualMatch";
/**
 * How text is inserted. `keyboard` synthesizes key events with the unicode string; `clipboard` swaps the pasteboard, pastes, and restores it (with a fallback when synthetic key events are unreliable for CJK input).
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "TextInputMethod".
 */
export type TextInputMethod = "keyboard" | "clipboard";
/**
 * `trace.export` request — exporting a trace requires an observation or control token.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "TraceExportParams".
 */
export type TraceExportParams =
  | {
      control_token?: string;
      dest?: string;
      observation_token: string;
      session_id?: string;
      [k: string]: unknown;
    }
  | {
      control_token: string;
      dest?: string;
      observation_token?: string;
      session_id?: string;
      [k: string]: unknown;
    };
/**
 * `trace.get` / `trace.replay` request — trace contents are a sensitive read; an observation or control token is required.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "TraceGetParams".
 */
export type TraceGetParams =
  | {
      control_token?: string;
      observation_token: string;
      session_id?: string;
      [k: string]: unknown;
    }
  | {
      control_token: string;
      observation_token?: string;
      session_id?: string;
      [k: string]: unknown;
    };
/**
 * How strictly traces are recorded.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "TraceMode".
 */
export type TraceMode = "Required" | "BestEffort" | "Disabled";
/**
 * `trace.replay` request (token-verified like `trace.get`).
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "TraceReplayParams".
 */
export type TraceReplayParams =
  | {
      control_token?: string;
      observation_token: string;
      session_id?: string;
      [k: string]: unknown;
    }
  | {
      control_token: string;
      observation_token?: string;
      session_id?: string;
      [k: string]: unknown;
    };

/**
 * JSON-RPC 2.0 wire protocol between the Computer Use daemon and its adapters (SDK, MCP server, Pi extension). Generated from the Rust wire types — edit the Rust source, then run `pnpm generate:protocol`. Capability tokens (control / observation / admin) are 256-bit secrets issued exactly once; the daemon stores only their SHA-256 hashes and never repeats them in responses.
 */
export interface ComputerUseProtocolV3 {
  [k: string]: unknown;
}
/**
 * `computer.act` request.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "ActParams".
 */
export interface ActParams {
  actions: ComputerAction[];
  /**
   * The session's control token. Required: without it the batch is rejected before any action is parsed, queued, or executed.
   */
  control_token: string;
  fixed_wait_ms?: number;
  frame_id: string;
  policy_context?: string;
  requires_confirmation?: boolean;
  return_screenshot?: boolean;
  risk_level?: string;
  session_id: string;
  wait_policy?: WaitPolicy;
  [k: string]: unknown;
}
/**
 * A 2D point in some coordinate space.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "Point".
 */
export interface Point {
  x: number;
  y: number;
  [k: string]: unknown;
}
/**
 * `computer.act` result.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "ActResult".
 */
export interface ActResult {
  action_results: ActionResultReport[];
  executed: boolean;
  next_frame_id?: string;
  screen_changed: boolean;
  screenshot?: ObserveResult;
  stabilization?: StabilizationInfo;
  stable: boolean;
  trace?: TraceReport;
  [k: string]: unknown;
}
/**
 * Result of one action inside a batch.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "ActionResultReport".
 */
export interface ActionResultReport {
  duration_ms: number;
  error?: string;
  index: number;
  status: string;
  [k: string]: unknown;
}
/**
 * `computer.observe` result.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "ObserveResult".
 */
export interface ObserveResult {
  active_application?: string;
  active_window?: string;
  captured_at: string;
  display_id: string;
  frame_id: string;
  height: number;
  /**
   * Base64-encoded image (only present when the caller requested it).
   */
  image_base64?: string;
  image_mime_type: string;
  /**
   * Absolute path to the stored image file.
   */
  image_path: string;
  scale_factor: number;
  session_id: string;
  width: number;
  [k: string]: unknown;
}
/**
 * Outcome of the post-batch stabilization wait (`WaitPolicy::UntilStable`). `change_score` is the **last measured** thumbnail difference — on timeout it carries the real score (never a fabricated 0), so the caller can tell a screen that nearly settled from one that kept animating.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "StabilizationInfo".
 */
export interface StabilizationInfo {
  change_score: number;
  elapsed_ms?: number;
  outcome: string;
  samples: number;
  [k: string]: unknown;
}
/**
 * Trace-recording status for this batch. Present when the session has a recorder; `degraded`/`warnings` surface best-effort recording problems so callers know the trace may be incomplete.
 */
export interface TraceReport {
  /**
   * True when the trace could not be written in best-effort mode (or the recorder degraded); the operation itself still succeeded.
   */
  degraded: boolean;
  /**
   * "required" | "best_effort" | "disabled" — the daemon's trace mode.
   */
  mode: string;
  /**
   * Human-readable warnings produced while recording this batch.
   */
  warnings?: string[];
  [k: string]: unknown;
}
/**
 * Detail attached to a [`CuError::OutOfBounds`].
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "BoundsDetail".
 */
export interface BoundsDetail {
  coordinate_space: string;
  image_height: number;
  image_width: number;
  x: number;
  y: number;
  [k: string]: unknown;
}
/**
 * `computer.cancel` request.
 *
 * Cancellation is **precise**: with `request_id` set, only the request with that JSON-RPC id (on the *same connection*) is cancelled — the runtime keys the in-flight batch by `(connection_id, request_id)`, so cancelling request A never touches request B, and client A can never cancel client B's request even with an identical id. Without `request_id` the whole session's in-flight batch is cancelled (still token-verified).
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "CancelParams".
 */
export interface CancelParams {
  /**
   * The session's control token — required; cancelling is a mutating op.
   */
  control_token: string;
  request_id?: unknown;
  session_id: string;
  [k: string]: unknown;
}
/**
 * `computer.cancel` result.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "CancelResult".
 */
export interface CancelResult {
  cancelled: boolean;
  session_id: string;
  [k: string]: unknown;
}
/**
 * Identity of the client that started a session. Recorded on the session so owners can be told apart: a client must not stop a session it did not start. `client_instance_id` distinguishes multiple processes of the same client (e.g. two Pi instances).
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "ClientInfo".
 */
export interface ClientInfo {
  client_id: string;
  client_instance_id: string;
  client_name: string;
  [k: string]: unknown;
}
/**
 * Detail attached to a [`CuError::ConfirmationRequired`].
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "ConfirmationDetail".
 */
export interface ConfirmationDetail {
  policy_context?: string;
  reason: string;
  requires_confirmation: boolean;
  risk_level: string;
  [k: string]: unknown;
}
/**
 * Mapping info that lets a model safely translate an inspect-relative coordinate back into global desktop coordinates.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "InspectMapping".
 */
export interface InspectMapping {
  /**
   * Global desktop point corresponding to the crop's top-left corner.
   *
   * @minItems 2
   * @maxItems 2
   */
  global_origin: never[];
  /**
   * The crop's top-left corner in the original frame's normalized_1000 space.
   *
   * @minItems 2
   * @maxItems 2
   */
  normalized_1000_origin: never[];
  source_image_rect: Region;
  [k: string]: unknown;
}
/**
 * Where the crop sits in the original image (pixels).
 */
export interface Region {
  coordinate_space: CoordinateSpace;
  height: number;
  width: number;
  x: number;
  y: number;
  [k: string]: unknown;
}
/**
 * A rectangle expressed in one of the image-relative coordinate spaces.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "Region".
 */
export interface Region1 {
  coordinate_space: CoordinateSpace;
  height: number;
  width: number;
  x: number;
  y: number;
  [k: string]: unknown;
}
/**
 * `computer.inspect` result.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "InspectResult".
 */
export interface InspectResult {
  frame_id: string;
  height: number;
  image_base64: string;
  image_mime_type: string;
  mapping: InspectMapping;
  session_id: string;
  width: number;
  [k: string]: unknown;
}
/**
 * Extra structured detail attached to a [`CuError::Permission`].
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "PermissionIssue".
 */
export interface PermissionIssue {
  granted: boolean;
  guidance: string;
  kind: PermissionKind;
  [k: string]: unknown;
}
/**
 * Redacted description of a `type` action used in traces and logs by default.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "RedactedText".
 */
export interface RedactedText {
  character_count: number;
  text_redacted: boolean;
  [k: string]: unknown;
}
/**
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "RpcError".
 */
export interface RpcError {
  code: number;
  data?: unknown;
  message: string;
  [k: string]: unknown;
}
/**
 * A JSON-RPC 2.0 request as received by the daemon.
 *
 * `Debug` is redacting by hand: `params` may carry capability tokens (`control_token` / `observation_token` / `admin_token`), so the derived form would print them. `{request:?}` in a log is always safe.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "RpcRequest".
 */
export interface RpcRequest {
  id?: unknown;
  jsonrpc: string;
  method: string;
  params?: unknown;
  [k: string]: unknown;
}
/**
 * A JSON-RPC 2.0 response written by the daemon.
 *
 * `Debug` redacts `result`/`error.data` (the one-time `start` response carries both capability tokens in `result`).
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "RpcResponse".
 */
export interface RpcResponse {
  error?: RpcError;
  id?: unknown;
  jsonrpc: string;
  result?: unknown;
  [k: string]: unknown;
}
/**
 * `runtime.version` result — the protocol-version contract.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "RuntimeVersionResult".
 */
export interface RuntimeVersionResult {
  maximum_client_protocol_version: number;
  /**
   * Inclusive lower bound of the client protocol versions this daemon accepts. A client below this (or above `maximum_client_protocol_version`) gets `PROTOCOL_VERSION_MISMATCH`.
   */
  minimum_client_protocol_version: number;
  name: string;
  protocol_version: number;
  /**
   * Wire name is `runtime_version` (the protocol spec's field name); the Rust field stays `version` to avoid `version.version`-style confusion.
   */
  runtime_version: string;
  [k: string]: unknown;
}
/**
 * `computer.session` result (shape depends on `action`).
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "SessionResult".
 */
export interface SessionResult {
  /**
   * The session's control token. **Only present in the `start` response** — it is issued exactly once, on creation, and never repeated by `status` or any other read-only call. Keep it in memory (or the CLI's 0600 credential file), never in logs or traces.
   */
  control_token?: string;
  created_at: string;
  current_frame_id?: string;
  display_id: string;
  last_action_at?: string;
  lock_held: boolean;
  message?: string;
  /**
   * The session's observation token (read-only capability). **Only present in the `start` response**, like the control token. A holder of only this token can observe/inspect/read traces, but can never act, cancel, pause, or stop.
   */
  observation_token?: string;
  /**
   * Who created this session (backward-compatible name of the starting client). The owner_* fields carry the structured identity; only the creating client may stop the session on exit.
   */
  owner_client_id?: string;
  owner_client_name?: string;
  owner_instance_id?: string;
  paused: boolean;
  session_id: string;
  started_by: string;
  state: SessionState;
  trace_dir?: string;
  user_takeover: boolean;
  [k: string]: unknown;
}
/**
 * `session.summary` — the **public** view of the active session. No token needed: it exposes only coarse state and non-secret owner identity. Full `status` (which includes `display_id`, `frame_id`, `trace_dir`) requires an observation or control token.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "SessionSummary".
 */
export interface SessionSummary {
  /**
   * True when the session is the control-lock holder.
   */
  lock_held: boolean;
  /**
   * Human-readable hint for the common case: the active session is owned by another client.
   */
  message: string | null;
  /**
   * The non-secret identity of the creating client (name only — never a token, never an instance id or frame/trace paths).
   */
  owner_client_id: string | null;
  owner_client_name: string | null;
  /**
   * `null` when no session exists — every field is always present on the wire (explicit nulls, never omitted keys): consumers read `summary.session_id == null` without juggling absence.
   */
  session_id: string | null;
  state: SessionState | null;
  [k: string]: unknown;
}
/**
 * `runtime.shutdown` request — requires the daemon admin token.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "ShutdownParams".
 */
export interface ShutdownParams {
  /**
   * The daemon admin token (per-install credential held by the daemon manager — the CLI / LaunchAgent). Ordinary clients never hold it.
   */
  admin_token: string;
  [k: string]: unknown;
}
/**
 * Detail attached to a [`CuError::StaleFrame`].
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "StaleFrameDetail".
 */
export interface StaleFrameDetail {
  change_score: number;
  current_frame_id: string;
  reason: string;
  referenced_frame_id: string;
  [k: string]: unknown;
}
/**
 * One entry inside a trace file (JSONL).
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "TraceEntry".
 */
export interface TraceEntry {
  action?: unknown;
  active_application?: string;
  change_score?: number;
  display_id?: string;
  duration_ms?: number;
  error?: unknown;
  event: string;
  frame_id?: string;
  redaction?: RedactedText;
  request_id?: string;
  result?: unknown;
  runtime_version?: string;
  seq: number;
  session_id?: string;
  stable?: boolean;
  ts: string;
  [k: string]: unknown;
}
/**
 * `trace.export` result.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "TraceExport".
 */
export interface TraceExport {
  exported_at: string;
  format: string;
  path: string;
  session_id: string;
  [k: string]: unknown;
}
/**
 * Recording status of the session's trace for one `computer.act` batch.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "TraceReport".
 */
export interface TraceReport1 {
  /**
   * True when the trace could not be written in best-effort mode (or the recorder degraded); the operation itself still succeeded.
   */
  degraded: boolean;
  /**
   * "required" | "best_effort" | "disabled" — the daemon's trace mode.
   */
  mode: string;
  /**
   * Human-readable warnings produced while recording this batch.
   */
  warnings?: string[];
  [k: string]: unknown;
}
/**
 * `trace.list` / `trace.summaries` entry — metadata only. The absolute filesystem path never crosses the wire (a path would leak the install layout and invite path-based probing); contents are read via `trace.get` and exported via `trace.export`, both token-gated.
 *
 * This interface was referenced by `ComputerUseProtocolV3`'s JSON-Schema
 * via the `definition` "TraceSummary".
 */
export interface TraceSummary {
  created_at: string;
  event_count: number;
  session_id: string;
  size_bytes: number;
  /**
   * Stable id of this trace. One trace per session — `trace_id` is the trace-file stem, which is currently the session id.
   */
  trace_id: string;
  [k: string]: unknown;
}

export const PROTOCOL_VERSION = 3;
export const MINIMUM_CLIENT_PROTOCOL_VERSION = 3;
export const MAXIMUM_CLIENT_PROTOCOL_VERSION = 3;
export const JSONRPC_VERSION = "2.0";
