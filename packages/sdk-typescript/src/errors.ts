// Error taxonomy of the computer-use protocol.
//
// The daemon speaks JSON-RPC 2.0. The top-level `error.code` is a numeric
// JSON-RPC code (-32000..=-32099 for application errors); `error.data` carries
// a machine-readable `{ code: "STALE_FRAME", message: "...", ...detail }`
// object built by cu-core's CuError. We surface both.
//
// The names below are the stable machine codes agents should program
// against. A few codes alias the wire strings the daemon has always used
// (e.g. `PERMISSION_DENIED` is the name agents should use for the daemon's
// `PERMISSION`); aliases keep one canonical name per failure while
// preserving the wire protocol.

/** Machine-readable error codes as returned in `error.data.code`. */
export const ERROR_CODES = {
  PARSE_ERROR: "PARSE_ERROR",
  INVALID_REQUEST: "INVALID_REQUEST",
  METHOD_NOT_FOUND: "METHOD_NOT_FOUND",
  INVALID_PARAMS: "INVALID_PARAMS",
  INTERNAL: "INTERNAL",
  NOT_READY: "NOT_READY",
  // Daemon unreachable (connection-level; see TransportError).
  DAEMON_UNAVAILABLE: "DAEMON_UNAVAILABLE",
  PERMISSION: "PERMISSION",
  /** Canonical name for a missing macOS permission (wire code: PERMISSION). */
  PERMISSION_DENIED: "PERMISSION",
  STALE_FRAME: "STALE_FRAME",
  CONTROL_LOCKED: "CONTROL_LOCKED",
  PAUSED: "PAUSED",
  /** Canonical name for a paused session (wire code: PAUSED). */
  SESSION_PAUSED: "PAUSED",
  USER_TAKEOVER: "USER_TAKEOVER",
  /** Resume was attempted while the user holds control; call release first. */
  USER_TAKEOVER_ACTIVE: "USER_TAKEOVER_ACTIVE",
  OUT_OF_BOUNDS: "OUT_OF_BOUNDS",
  UNKNOWN_FRAME: "UNKNOWN_FRAME",
  SESSION_NOT_FOUND: "SESSION_NOT_FOUND",
  INVALID_SESSION_STATE: "INVALID_SESSION_STATE",
  CONFIRMATION_REQUIRED: "CONFIRMATION_REQUIRED",
  CANCELLED: "CANCELLED",
  /** Canonical name for a cancelled action (wire code: CANCELLED). */
  ACTION_CANCELLED: "CANCELLED",
  /** A request/batch exceeded the daemon's deadline (distinct from cancel). */
  ACTION_TIMEOUT: "ACTION_TIMEOUT",
  /** Screen capture failed (driver/capture failure). */
  CAPTURE_FAILED: "CAPTURE_FAILED",
  /** A mutating operation was attempted without a session control token. */
  CONTROL_TOKEN_REQUIRED: "CONTROL_TOKEN_REQUIRED",
  /** A control token was presented but did not verify (never says why). */
  INVALID_CONTROL_TOKEN: "INVALID_CONTROL_TOKEN",
  /** A mutating operation targeted a session that is already stopped. */
  SESSION_STOPPED: "SESSION_STOPPED",
  /** SDK-side request deadline (local). See RequestTimeoutError. */
  REQUEST_TIMEOUT: "REQUEST_TIMEOUT",
  /** The daemon speaks a different wire protocol version than this SDK. */
  PROTOCOL_VERSION_MISMATCH: "PROTOCOL_VERSION_MISMATCH",
  TRACE_ERROR: "TRACE_ERROR",
  /** Canonical name for trace unavailability (wire code: TRACE_ERROR). */
  TRACE_UNAVAILABLE: "TRACE_ERROR",
  DRIVER_ERROR: "DRIVER_ERROR",
  UNSUPPORTED: "UNSUPPORTED",
} as const;

export type ErrorCodeName = (typeof ERROR_CODES)[keyof typeof ERROR_CODES];

const JSONRPC_CODE_TO_NAME: Record<number, ErrorCodeName> = {
  [-32700]: ERROR_CODES.PARSE_ERROR,
  [-32600]: ERROR_CODES.INVALID_REQUEST,
  [-32601]: ERROR_CODES.METHOD_NOT_FOUND,
  [-32602]: ERROR_CODES.INVALID_PARAMS,
  [-32000]: ERROR_CODES.INTERNAL,
  [-32001]: ERROR_CODES.NOT_READY,
  [-32002]: ERROR_CODES.PERMISSION,
  [-32003]: ERROR_CODES.STALE_FRAME,
  [-32004]: ERROR_CODES.CONTROL_LOCKED,
  [-32005]: ERROR_CODES.PAUSED,
  [-32006]: ERROR_CODES.USER_TAKEOVER,
  [-32007]: ERROR_CODES.OUT_OF_BOUNDS,
  [-32008]: ERROR_CODES.UNKNOWN_FRAME,
  [-32009]: ERROR_CODES.SESSION_NOT_FOUND,
  [-32010]: ERROR_CODES.INVALID_SESSION_STATE,
  [-32011]: ERROR_CODES.CONFIRMATION_REQUIRED,
  [-32012]: ERROR_CODES.CANCELLED,
  [-32013]: ERROR_CODES.TRACE_ERROR,
  [-32014]: ERROR_CODES.DRIVER_ERROR,
  [-32015]: ERROR_CODES.UNSUPPORTED,
  [-32016]: ERROR_CODES.USER_TAKEOVER_ACTIVE,
  [-32017]: ERROR_CODES.ACTION_TIMEOUT,
  [-32018]: ERROR_CODES.CAPTURE_FAILED,
  [-32019]: ERROR_CODES.CONTROL_TOKEN_REQUIRED,
  [-32020]: ERROR_CODES.INVALID_CONTROL_TOKEN,
  [-32021]: ERROR_CODES.SESSION_STOPPED,
  [-32022]: ERROR_CODES.REQUEST_TIMEOUT,
  [-32023]: ERROR_CODES.PROTOCOL_VERSION_MISMATCH,
};

/** Map a numeric JSON-RPC code to its machine-readable name. */
export function errorCodeName(jsonrpcCode: number): ErrorCodeName {
  return (
    JSONRPC_CODE_TO_NAME[jsonrpcCode] ?? ERROR_CODES.INTERNAL
  );
}

/** A typed error returned by the daemon. */
export class ComputerUseError extends Error {
  /** Machine-readable code, e.g. `"STALE_FRAME"` (from `error.data.code`). */
  readonly code: ErrorCodeName;
  /** Numeric JSON-RPC code from the top-level `error.code`. */
  readonly jsonrpcCode: number;
  /** Raw `error.data` payload (may carry `permission`, `bounds`, `frame_id`s). */
  readonly data: unknown;

  constructor(jsonrpcCode: number, message: string, data: unknown) {
    const dataCode = (data as { code?: unknown } | null)?.code;
    const code =
      typeof dataCode === "string"
        ? (dataCode as ErrorCodeName)
        : errorCodeName(jsonrpcCode);
    super(message || code);
    this.name = "ComputerUseError";
    this.code = code;
    this.jsonrpcCode = jsonrpcCode;
    this.data = data;
  }
}

/** Connection-level failures (socket, framing, timeouts). */
export class TransportError extends Error {
  /** `"DAEMON_UNAVAILABLE"` when the daemon could not be reached. */
  readonly code: string;
  constructor(message: string) {
    super(message);
    this.name = "TransportError";
    this.code = "DAEMON_UNAVAILABLE";
  }
}

/**
 * The SDK's own request deadline expired (local timeout, not a daemon error).
 *
 * The SDK does not just give up: it sends a precise `computer.cancel` (same
 * connection, same `request_id`, with the session's control token) and waits
 * for the daemon's acknowledgement before resolving this error.
 * `runtimeCancellationConfirmed` is `true` only when the daemon explicitly
 * reported the batch as cancelled — the SDK never claims the runtime stopped
 * when it did not receive the acknowledgement.
 */
export class RequestTimeoutError extends TransportError {
  /** Always `"REQUEST_TIMEOUT"` — the SDK-side deadline expired. */
  override readonly code = "REQUEST_TIMEOUT" as const;
  /** True when the runtime confirmed the request was cancelled. */
  readonly runtimeCancellationConfirmed: boolean;
  constructor(message: string, runtimeCancellationConfirmed: boolean) {
    super(message);
    this.name = "RequestTimeoutError";
    this.runtimeCancellationConfirmed = runtimeCancellationConfirmed;
  }
}

/** The caller aborted the request via an AbortSignal (local cancellation, not a daemon error). */
export class AbortError extends Error {
  constructor(message = "The operation was aborted") {
    super(message);
    this.name = "AbortError";
  }
}
