// Wire types of the computer-use JSON-RPC protocol.
//
// Single source of truth: the Rust wire types (crates/cu-core/src/protocol.rs,
// actions.rs, coordinates.rs, sessions.rs, errors.rs). `pnpm generate:protocol`
// emits protocol/computer-use.schema.json from them, and this package's
// src/generated/protocol.ts from that schema. **Never hand-edit a wire type** —
// run `pnpm generate:protocol` and commit the result (`pnpm check:protocol`
// fails on drift).
export * from "./generated/protocol.js";
// Local bindings for the SDK-domain types below (`export *` re-exports but
// does not bring names into scope).
import type { Point, TraceSummary } from "./generated/protocol.js";

// ---------------------------------------------------------------------------
// SDK-domain types (client-side, not part of the wire protocol)
// ---------------------------------------------------------------------------

/**
 * The capability credential this client holds for one session.
 *
 * - `observationToken`: read-only capability (observe / inspect / status /
 *   trace). Issued by `start`; also obtained via an explicit read-only attach.
 * - `controlToken`: full capability — control includes observation. Held only
 *   by the client that started the session (or attached with an explicit
 *   control token).
 * - `access`: what this credential may do — "read_only" or "control".
 */
export interface SessionCredential {
  sessionId: string;
  observationToken: string;
  controlToken?: string;
  /** Identity the owner reported when it started the session. */
  ownerClientId?: string;
  ownerInstanceId?: string;
  access: "read_only" | "control";
}

/** What to do when an active session exists that this client does not own. */
export type ExistingSessionPolicy = "reject" | "read_only" | "attach_with_token";

export interface DisplayBounds {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface TraceList {
  traces: TraceSummary[];
}

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
