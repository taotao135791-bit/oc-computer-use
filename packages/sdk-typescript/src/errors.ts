// Error taxonomy of the computer-use protocol.
//
// The daemon speaks JSON-RPC 2.0. The top-level `error.code` is a numeric
// JSON-RPC code (-32000..=-32015 for application errors); `error.data` carries
// a machine-readable `{ code: "STALE_FRAME", message: "...", ...detail }`
// object built by cu-core's CuError. We surface both.

/** Machine-readable error codes as returned in `error.data.code`. */
export const ERROR_CODES = {
  PARSE_ERROR: "PARSE_ERROR",
  INVALID_REQUEST: "INVALID_REQUEST",
  METHOD_NOT_FOUND: "METHOD_NOT_FOUND",
  INVALID_PARAMS: "INVALID_PARAMS",
  INTERNAL: "INTERNAL",
  NOT_READY: "NOT_READY",
  PERMISSION: "PERMISSION",
  STALE_FRAME: "STALE_FRAME",
  CONTROL_LOCKED: "CONTROL_LOCKED",
  PAUSED: "PAUSED",
  USER_TAKEOVER: "USER_TAKEOVER",
  OUT_OF_BOUNDS: "OUT_OF_BOUNDS",
  UNKNOWN_FRAME: "UNKNOWN_FRAME",
  SESSION_NOT_FOUND: "SESSION_NOT_FOUND",
  INVALID_SESSION_STATE: "INVALID_SESSION_STATE",
  CONFIRMATION_REQUIRED: "CONFIRMATION_REQUIRED",
  CANCELLED: "CANCELLED",
  TRACE_ERROR: "TRACE_ERROR",
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
  constructor(message: string) {
    super(message);
    this.name = "TransportError";
  }
}
