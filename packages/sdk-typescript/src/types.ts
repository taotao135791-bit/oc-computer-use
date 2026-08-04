// Wire types of the computer-use JSON-RPC protocol.
//
// Hand-written from crates/cu-core/src/protocol.rs, actions.rs, coordinates.rs,
// sessions.rs and crates/cu-driver/src/types.rs. Keep them in sync with the
// Rust definitions: serde field names are snake_case, enums serialize with
// `rename_all = "snake_case"`, `Option` fields are omitted when absent, and
// `ComputerAction` is a tagged union on `type`.

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

/** Coordinate space used when describing a location or region. */
export type CoordinateSpace = "normalized_1000" | "image_pixels";

/**
 * Wire protocol version of this SDK. Round 3 introduced server-side session
 * ownership (control tokens), a breaking protocol change: this SDK checks the
 * daemon's `runtime.version.protocol_version` on connect and refuses to talk
 * to daemons with a different version.
 */
export const PROTOCOL_VERSION = 2;

export interface Point {
  x: number;
  y: number;
}

export interface Region {
  x: number;
  y: number;
  width: number;
  height: number;
  coordinate_space: CoordinateSpace;
}

export interface DisplayBounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

export type MouseButton = "left" | "right" | "middle";

/** How text is inserted: synthetic key events or pasteboard + paste. */
export type TextInputMethod = "keyboard" | "clipboard";

/** What the runtime should do after executing an action batch. */
export type WaitPolicy = "none" | "fixed" | "until_stable";

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/**
 * A single atomic computer action. Tagged union on `type`, exactly as the
 * runtime serializes it (serde tag = "type", rename_all = "snake_case").
 */
export type ComputerAction =
  | {
      type: "click";
      x: number;
      y: number;
      button: MouseButton;
      coordinate_space: CoordinateSpace;
    }
  | {
      type: "double_click";
      x: number;
      y: number;
      button: MouseButton;
      coordinate_space: CoordinateSpace;
    }
  | {
      type: "move";
      x: number;
      y: number;
      coordinate_space: CoordinateSpace;
      /** Animation duration in ms (defaults to the driver's default). */
      duration_ms?: number;
    }
  | {
      /** Note: the wire tag is `type` (serde rename), not `type_text`. */
      type: "type";
      text: string;
      method?: TextInputMethod;
    }
  | { type: "key"; keys: string[] }
  | {
      type: "scroll";
      x?: number;
      y?: number;
      /** Scroll deltas; absent defaults to 0 (macOS: pixels per notch). */
      delta_x?: number;
      delta_y?: number;
      coordinate_space: CoordinateSpace;
    }
  | {
      type: "drag";
      from: Point;
      to: Point;
      coordinate_space: CoordinateSpace;
      duration_ms?: number;
    }
  | { type: "wait"; duration_ms: number };

// ---------------------------------------------------------------------------
// computer.session
// ---------------------------------------------------------------------------

export type SessionAction =
  | "start"
  | "status"
  | "pause"
  | "resume"
  | "stop"
  | "takeover"
  | "release";

export type SessionState =
  | "starting"
  | "active"
  | "paused"
  | "user_takeover"
  | "stopping"
  | "stopped"
  | "failed";

export interface SessionParams {
  action: SessionAction;
  /** Optional for `status` (resolves the active session); required elsewhere. */
  session_id?: string;
  display_id?: string;
  /** Identity recorded on the session when `start` creates it. */
  client_id?: string;
  client_name?: string;
  client_instance_id?: string;
  /**
   * The session's control token. Required for every mutating action
   * (`pause`/`resume`/`takeover`/`release`/`stop`); never sent for `start`
   * (which issues the token) or `status` (read-only). The SDK injects the
   * token it holds automatically — only a caller that never started the
   * session is missing it, and the daemon will say CONTROL_TOKEN_REQUIRED.
   */
  control_token?: string;
}

/**
 * The capability that grants control of a session. Issued **once**, in the
 * `start` response; the daemon stores only a hash of it and never repeats it.
 * Knowing a session id grants nothing — a client without this token cannot
 * act, cancel, pause, resume, take over, release, or stop the session.
 */
export interface SessionCredential {
  sessionId: string;
  controlToken: string;
  /** Identity the owner reported when it started the session. */
  ownerClientId?: string;
  ownerInstanceId?: string;
}

/** What to do when an active session exists that this client does not own. */
export type ExistingSessionPolicy = "reject" | "read_only" | "attach_with_token";

/**
 * Identity of the client that started a session. The owner is the only client
 * that may stop a session it created; other clients attach without stopping.
 */
export interface ClientInfo {
  client_id: string;
  client_name: string;
  client_instance_id: string;
}

export interface SessionResult {
  session_id: string;
  state: SessionState;
  paused: boolean;
  user_takeover: boolean;
  lock_held: boolean;
  display_id: string;
  created_at: string;
  last_action_at?: string;
  current_frame_id?: string;
  trace_dir?: string;
  started_by: string;
  /** Who created the session (absent on sessions started by anonymous clients). */
  owner_client_id?: string;
  owner_client_name?: string;
  owner_instance_id?: string;
  message?: string;
  /**
   * The session's control token — **only present in the `start` response**,
   * and never in `status` or any other read-only result. Keep it in memory
   * (or a 0600 credential file), never in logs.
   */
  control_token?: string;
}

// ---------------------------------------------------------------------------
// computer.observe
// ---------------------------------------------------------------------------

export interface ObserveParams {
  session_id?: string;
  /** Primary display is used when omitted. */
  display_id?: string;
  include_cursor?: boolean;
  /** Downscale so the image width does not exceed this. */
  max_width?: number;
  image_format?: "png" | "jpeg";
  jpeg_quality?: number;
  /** Return the image as base64 in the response (off by default). */
  include_image?: boolean;
}

export interface ObserveResult {
  session_id: string;
  frame_id: string;
  width: number;
  height: number;
  display_id: string;
  scale_factor: number;
  active_application?: string;
  active_window?: string;
  /** Only present when `include_image` was requested. */
  image_base64?: string;
  /** Absolute path of the stored image file. */
  image_path: string;
  image_mime_type: string;
  captured_at: string;
}

// ---------------------------------------------------------------------------
// computer.act
// ---------------------------------------------------------------------------

export interface ActParams {
  session_id: string;
  /** The frame the model is acting on; must not be stale. */
  frame_id: string;
  actions: ComputerAction[];
  wait_policy?: WaitPolicy;
  /** Only honored with `wait_policy: "fixed"`. */
  fixed_wait_ms?: number;
  return_screenshot?: boolean;
  // Forward-looking safety hooks (accepted, documented, not enforced in v1).
  risk_level?: string;
  requires_confirmation?: boolean;
  policy_context?: string;
  /**
   * The session's control token. Required — the daemon refuses the batch
   * before executing anything without it. The SDK injects the token it holds
   * automatically; a caller that is not the session's owner cannot act.
   */
  control_token?: string;
}

export interface ActionResultReport {
  index: number;
  status: "success" | "failed" | "cancelled";
  duration_ms: number;
  error?: string;
}

/**
 * Outcome of the post-batch stabilization wait (`wait_policy: "until_stable"`).
 * On timeout `change_score` is the **last measured** thumbnail difference —
 * never a fabricated 0 — so the caller can tell a screen that nearly settled
 * from one that kept animating.
 */
export interface StabilizationInfo {
  outcome: "stable" | "timed_out";
  change_score: number;
  samples: number;
  elapsed_ms?: number;
}

/** Trace-recording status for an act batch. */
export interface TraceReport {
  mode: "required" | "best_effort" | "disabled";
  /** True when the trace could not be written (best-effort mode degraded). */
  degraded: boolean;
  warnings: string[];
}

export interface ActResult {
  executed: boolean;
  action_results: ActionResultReport[];
  screen_changed: boolean;
  stable: boolean;
  next_frame_id?: string;
  /** Only present when `return_screenshot` was requested. */
  screenshot?: ObserveResult;
  /** Present when `wait_policy: "until_stable"` was requested. */
  stabilization?: StabilizationInfo;
  /** Present when the session records traces. */
  trace?: TraceReport;
}

// ---------------------------------------------------------------------------
// computer.inspect
// ---------------------------------------------------------------------------

export interface InspectParams {
  session_id: string;
  frame_id: string;
  region: Region;
  /** Integer scale applied to the crop (1 = 100%). */
  scale?: number;
}

export interface InspectMapping {
  /** Where the crop sits in the original image (pixels). */
  source_image_rect: Region;
  /** Global desktop point of the crop's top-left corner. */
  global_origin: [number, number];
  /** Crop top-left in the original frame's normalized_1000 space. */
  normalized_1000_origin: [number, number];
}

export interface InspectResult {
  session_id: string;
  frame_id: string;
  width: number;
  height: number;
  image_base64: string;
  image_mime_type: string;
  mapping: InspectMapping;
}

// ---------------------------------------------------------------------------
// Trace management
// ---------------------------------------------------------------------------

export interface RedactedText {
  text_redacted: boolean;
  character_count: number;
}

export interface TraceSummary {
  session_id: string;
  path: string;
  entries: number;
  bytes: number;
  started_at: string;
  last_entry_at?: string;
}

export interface TraceEntry {
  seq: number;
  ts: string;
  event: string;
  session_id?: string;
  request_id?: string;
  frame_id?: string;
  action?: unknown;
  result?: unknown;
  duration_ms?: number;
  error?: unknown;
  change_score?: number;
  stable?: boolean;
  /** Present on `type` actions when text was redacted (the default). */
  redaction?: RedactedText;
  display_id?: string;
  active_application?: string;
  runtime_version?: string;
}

export interface TraceExport {
  session_id: string;
  path: string;
  format: string;
  exported_at: string;
}

export interface TraceList {
  traces: TraceSummary[];
}

// ---------------------------------------------------------------------------
// Runtime introspection
// ---------------------------------------------------------------------------

export interface PermissionStatus {
  screen_recording: boolean;
  accessibility: boolean;
}

export interface Health {
  version: string;
  ready: boolean;
  permissions: PermissionStatus;
  active_sessions: number;
  uptime_secs: number;
  frame_cache: number;
}

export interface RuntimeVersion {
  name: string;
  version: string;
  /** Wire protocol version of the daemon. Absent on pre-round-3 daemons. */
  protocol_version?: number;
}

export interface DisplayInfo {
  id: string;
  name: string;
  bounds: DisplayBounds;
  pixel_width: number;
  pixel_height: number;
  scale_factor: number;
  is_main: boolean;
}

export interface DesktopLayout {
  displays: DisplayInfo[];
  primary_id: string;
}

export interface ApplicationInfo {
  bundle_id: string;
  name: string;
  window_title?: string;
}

export interface PointerInfo {
  location: Point;
  display_id?: string;
}

// ---------------------------------------------------------------------------
// computer.cancel
// ---------------------------------------------------------------------------

/**
 * Cancellation is **precise**: with `request_id` set, only the request with
 * that JSON-RPC id (on the *same connection*) is cancelled. Two clients may
 * both use `request_id: 1` and cancelling one never touches the other. Without
 * `request_id` the whole session's in-flight batch is cancelled. Either way a
 * valid `control_token` is required.
 */
export interface CancelParams {
  session_id: string;
  /** The session's control token — required; cancelling is a mutating op. */
  control_token?: string;
  /** JSON-RPC id of the specific request to cancel (same connection). */
  request_id?: number;
}

export interface CancelResult {
  cancelled: boolean;
  session_id: string;
}
