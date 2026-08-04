// JSON-RPC 2.0 client for the computer-use daemon.
//
// Transport: newline-delimited JSON over a Unix domain socket
// (`~/.computer-use/runtime.sock` by default). Zero runtime dependencies —
// only Node built-ins (`node:net`, `node:path`, `node:os`).
//
// Requests may be pipelined; responses are matched to requests by `id`, so
// out-of-order responses from the daemon are handled correctly.

import { createConnection, type Socket } from "node:net";
import { homedir } from "node:os";
import { join } from "node:path";

import { AbortError, ComputerUseError, TransportError } from "./errors.js";
import type {
  ActParams,
  ActResult,
  ApplicationInfo,
  ClientInfo,
  DesktopLayout,
  DisplayInfo,
  Health,
  InspectParams,
  InspectResult,
  ObserveParams,
  ObserveResult,
  PermissionStatus,
  PointerInfo,
  RuntimeVersion,
  SessionAction,
  SessionOnlyParams,
  SessionParams,
  SessionResult,
  TraceEntry,
  TraceExport,
  TraceList,
} from "./types.js";

/** Default socket path: `~/.computer-use/runtime.sock`. */
export function defaultSocketPath(): string {
  return join(homedir(), ".computer-use", "runtime.sock");
}

export interface ClientOptions {
  /** Unix socket path (defaults to `~/.computer-use/runtime.sock`). */
  socketPath?: string;
  /** Per-request timeout in ms (default 30_000). */
  timeoutMs?: number;
  /**
   * Identity this client reports when it starts a session. The daemon records
   * it on the session; the owner is the only client that may stop the session
   * it created. Defaults to a per-process SDK identity.
   */
  clientInfo?: ClientInfo;
}

/** Per-call overrides for a single JSON-RPC request. */
export interface RequestOptions {
  /** Override this client's default per-request timeout (ms). */
  timeoutMs?: number;
  /** Abort the request. Rejects with `AbortError` when it fires. */
  signal?: AbortSignal;
}

interface PendingRequest {
  resolve: (value: unknown) => void;
  reject: (err: Error) => void;
  timer: NodeJS.Timeout;
}

/**
 * A connected client to the computer-use daemon. One connection supports any
 * number of concurrent requests (matched by JSON-RPC id).
 */
export class ComputerUseClient {
  readonly socketPath: string;
  readonly defaultTimeoutMs: number;
  /** Identity used when this client starts a session. */
  readonly clientInfo: ClientInfo;

  private socket: Socket | null = null;
  private buffer = "";
  private pending = new Map<number, PendingRequest>();
  private nextId = 1;
  private closed = false;
  /** Single-flight guard: concurrent ensureSession() calls share one status→start. */
  private ensureSessionPromise: Promise<SessionResult> | null = null;

  constructor(opts: ClientOptions = {}) {
    this.socketPath = opts.socketPath ?? defaultSocketPath();
    this.defaultTimeoutMs = opts.timeoutMs ?? 30_000;
    this.clientInfo = opts.clientInfo ?? {
      client_id: "sdk",
      client_name: "TypeScript SDK",
      client_instance_id: `${process.pid}-${Math.random().toString(36).slice(2, 10)}`,
    };
  }

  /** Connect to the daemon socket. Rejects with TransportError on failure. */
  connect(timeoutMs: number = 10_000): Promise<this> {
    if (this.socket) return Promise.resolve(this);
    return new Promise((resolve, reject) => {
      const socket = createConnection(this.socketPath);
      this.socket = socket;
      this.closed = false;

      const onError = (err: NodeJS.ErrnoException) => {
        cleanup();
        this.socket = null;
        reject(
          new TransportError(
            `cannot connect to ${this.socketPath}: ${err.message} (is the daemon running? try \`cu daemon start\`)`,
          ),
        );
      };
      const onConnect = () => {
        cleanup();
        resolve(this);
      };
      const cleanup = () => {
        socket.off("connect", onConnect);
        socket.off("error", onError);
        clearTimeout(timer);
      };
      const timer = setTimeout(() => {
        socket.destroy();
        cleanup();
        reject(new TransportError(`connection to ${this.socketPath} timed out`));
      }, timeoutMs);
      timer.unref();

      socket.on("connect", onConnect);
      socket.on("error", onError);
      socket.on("data", (chunk) => this.onData(chunk));
      socket.on("close", () => {
        this.closed = true;
        this.socket = null;
        this.failAllPending(
          new TransportError("connection to daemon closed while requests were in flight"),
        );
      });
    });
  }

  /** Close the connection. Pending requests are rejected. */
  close(): void {
    if (this.socket) {
      this.socket.destroy();
      this.socket = null;
    }
    this.closed = true;
    this.failAllPending(new TransportError("client closed"));
  }

  /**
   * Send one JSON-RPC request and await its response.
   * Rejects with ComputerUseError for daemon errors, TransportError for
   * connection/framing failures, AbortError when `options.signal` fires.
   *
   * The third argument accepts either a bare timeout (ms) for backward
   * compatibility or a `RequestOptions` object.
   */
  request<T = unknown>(
    method: string,
    params?: unknown,
    options: RequestOptions | number = {},
  ): Promise<T> {
    const opts: RequestOptions = typeof options === "number" ? { timeoutMs: options } : options;
    if (this.closed || !this.socket) {
      return Promise.reject(
        new TransportError("not connected — call connect() first"),
      );
    }
    if (opts.signal?.aborted) {
      return Promise.reject(new AbortError(`request ${method} aborted before it started`));
    }
    const id = this.nextId++;
    const payload = { jsonrpc: "2.0", id, method, ...(params !== undefined ? { params } : {}) };
    const timerMs = opts.timeoutMs ?? this.defaultTimeoutMs;

    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        opts.signal?.removeEventListener("abort", onAbort);
        this.pending.delete(id);
        reject(
          new TransportError(`request ${method} timed out after ${timerMs}ms`),
        );
      }, timerMs);
      timer.unref();
      const onAbort = () => {
        clearTimeout(timer);
        this.pending.delete(id);
        // The full cancel chain: an aborted request also tells the daemon to
        // cancel the in-flight batch (fire-and-forget), so a long wait or
        // stabilizer stops server-side, not just in this process.
        if (method !== "computer.cancel") this.notifyCancel(params);
        reject(new AbortError(`request ${method} aborted by caller`));
      };
      opts.signal?.addEventListener("abort", onAbort, { once: true });

      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, timer });

      this.socket!.write(`${JSON.stringify(payload)}\n`, (err) => {
        if (err) {
          const pending = this.pending.get(id);
          if (pending) {
            clearTimeout(pending.timer);
            opts.signal?.removeEventListener("abort", onAbort);
            this.pending.delete(id);
            reject(new TransportError(`failed to write to daemon: ${err.message}`));
          }
        }
      });
    });
  }

  // -------------------------------------------------------------------------
  // Runtime introspection
  // -------------------------------------------------------------------------

  health(): Promise<Health> {
    return this.request<Health>("runtime.health");
  }

  version(): Promise<RuntimeVersion> {
    return this.request<RuntimeVersion>("runtime.version");
  }

  permissions(): Promise<PermissionStatus> {
    return this.request<PermissionStatus>("runtime.permissions");
  }

  displays(): Promise<DisplayInfo[]> {
    return this.request<DisplayInfo[]>("runtime.displays");
  }

  desktopLayout(): Promise<DesktopLayout> {
    return this.request<DesktopLayout>("runtime.desktop_layout");
  }

  pointer(): Promise<PointerInfo> {
    return this.request<PointerInfo>("runtime.pointer");
  }

  activeApplication(): Promise<ApplicationInfo> {
    return this.request<ApplicationInfo>("runtime.active_application");
  }

  shutdown(): Promise<{ status: string }> {
    return this.request<{ status: string }>("runtime.shutdown");
  }

  // -------------------------------------------------------------------------
  // Sessions
  // -------------------------------------------------------------------------

  session(
    action: SessionAction,
    params: Partial<SessionParams> = {},
    options?: RequestOptions,
  ): Promise<SessionResult> {
    const p: Partial<SessionParams> = { action, ...params };
    // A session start records its creator's identity (ownership). Callers that
    // do not pin their own identity get this client's.
    if (action === "start" && !p.client_id) {
      p.client_id = this.clientInfo.client_id;
      p.client_name = this.clientInfo.client_name;
      p.client_instance_id = this.clientInfo.client_instance_id;
    }
    return this.request<SessionResult>("computer.session", p, options ?? {});
  }

  /**
   * Ensure an active session exists: resolves the current one, or starts a new
   * one. Returns the resulting SessionResult.
   *
   * Single-flight: concurrent callers share one resolution, so two simultaneous
   * observe calls can never start two competing sessions.
   */
  ensureSession(
    displayId?: string,
    options?: RequestOptions,
    clientInfo?: ClientInfo,
  ): Promise<SessionResult> {
    if (!this.ensureSessionPromise) {
      this.ensureSessionPromise = this.ensureSessionInner(displayId, options, clientInfo).finally(
        () => {
          this.ensureSessionPromise = null;
        },
      );
    }
    return this.ensureSessionPromise;
  }

  private async ensureSessionInner(
    displayId?: string,
    options?: RequestOptions,
    clientInfo?: ClientInfo,
  ): Promise<SessionResult> {
    try {
      return await this.session("status", {}, options);
    } catch (err) {
      if (err instanceof ComputerUseError && err.code === "SESSION_NOT_FOUND") {
        return this.session(
          "start",
          {
            display_id: displayId,
            ...(clientInfo ? { ...clientInfo } : {}),
          },
          options,
        );
      }
      throw err;
    }
  }

  // -------------------------------------------------------------------------
  // Observe / act / inspect / cancel
  // -------------------------------------------------------------------------

  /**
   * Capture a frame. When `session_id` is omitted, the active session is
   * resolved automatically (a new one is started if none exists).
   */
  async observe(params: ObserveParams = {}, options?: RequestOptions): Promise<ObserveResult> {
    if (!params.session_id) {
      const session = await this.ensureSession(undefined, options);
      return this.request<ObserveResult>(
        "computer.observe",
        { ...params, session_id: session.session_id },
        options ?? {},
      );
    }
    return this.request<ObserveResult>("computer.observe", params, options ?? {});
  }

  act(params: ActParams, options?: RequestOptions): Promise<ActResult> {
    return this.request<ActResult>("computer.act", params, options ?? {});
  }

  inspect(params: InspectParams, options?: RequestOptions): Promise<InspectResult> {
    return this.request<InspectResult>("computer.inspect", params, options ?? {});
  }

  cancel(
    params: SessionOnlyParams,
    options?: RequestOptions,
  ): Promise<{ cancelled: boolean; session_id: string }> {
    return this.request<{ cancelled: boolean; session_id: string }>(
      "computer.cancel",
      params,
      options ?? {},
    );
  }

  // -------------------------------------------------------------------------
  // Traces
  // -------------------------------------------------------------------------

  traceList(): Promise<TraceList> {
    return this.request<TraceList>("trace.list");
  }

  traceGet(sessionId: string): Promise<TraceEntry[]> {
    return this.request<TraceEntry[]>("trace.get", { session_id: sessionId });
  }

  traceExport(sessionId: string, dest: string): Promise<TraceExport> {
    return this.request<TraceExport>("trace.export", { session_id: sessionId, dest });
  }

  traceReplay(sessionId: string): Promise<unknown> {
    return this.request<unknown>("trace.replay", { session_id: sessionId });
  }

  // -------------------------------------------------------------------------
  // Internals
  // -------------------------------------------------------------------------

  private onData(chunk: Buffer): void {
    this.buffer += chunk.toString("utf8");
    let nl: number;
    while ((nl = this.buffer.indexOf("\n")) >= 0) {
      const line = this.buffer.slice(0, nl).trim();
      this.buffer = this.buffer.slice(nl + 1);
      if (!line) continue;
      this.dispatchLine(line);
    }
    // Guard against unbounded buffering of a broken peer. High ceiling: an
    // include_image response legitimately carries a base64 screenshot.
    if (this.buffer.length > 64 * 1024 * 1024) {
      this.close();
    }
  }

  /**
   * Fire-and-forget `computer.cancel` for a session id found in request
   * params. Used when a request is aborted locally: the daemon cancels the
   * in-flight batch so a long wait/stabilizer stops server-side too. The
   * daemon's late error response for the original request is dropped — the
   * pending entry is already gone, and responses are matched by id.
   */
  private notifyCancel(params: unknown): void {
    if (!this.socket || this.closed) return;
    const sid = (params as { session_id?: unknown } | undefined)?.session_id;
    if (typeof sid !== "string" || !sid) return;
    const payload = {
      jsonrpc: "2.0",
      id: this.nextId++,
      method: "computer.cancel",
      params: { session_id: sid },
    };
    try {
      this.socket.write(`${JSON.stringify(payload)}\n`);
    } catch {
      // Socket is gone; the daemon side is unreachable either way.
    }
  }

  private dispatchLine(line: string): void {
    let msg: { id?: unknown; result?: unknown; error?: { code?: number; message?: string; data?: unknown } };
    try {
      msg = JSON.parse(line);
    } catch {
      // Not JSON — the daemon never emits this; drop it defensively.
      return;
    }
    const id = msg.id;
    if (typeof id !== "number") return;
    const pending = this.pending.get(id);
    if (!pending) return; // late response for a timed-out request
    clearTimeout(pending.timer);
    this.pending.delete(id);
    if (msg.error) {
      pending.reject(
        new ComputerUseError(
          msg.error.code ?? -32000,
          msg.error.message ?? "rpc error",
          msg.error.data,
        ),
      );
    } else {
      pending.resolve(msg.result);
    }
  }

  private failAllPending(err: TransportError): void {
    for (const [, pending] of this.pending) {
      clearTimeout(pending.timer);
      pending.reject(err);
    }
    this.pending.clear();
  }
}

/**
 * Connect to the daemon and return a ready client.
 * Shorthand for `new ComputerUseClient(opts).connect()`.
 */
export function connect(opts: ClientOptions = {}): Promise<ComputerUseClient> {
  return new ComputerUseClient(opts).connect();
}
