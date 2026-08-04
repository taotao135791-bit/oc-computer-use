// JSON-RPC 2.0 client for the computer-use daemon.
//
// Transport: newline-delimited JSON over a Unix domain socket
// (`~/.computer-use/runtime.sock` by default). Zero runtime dependencies —
// only Node built-ins (`node:net`, `node:path`, `node:os`).
//
// Requests may be pipelined; responses are matched to requests by `id`, so
// out-of-order responses from the daemon are handled correctly.
//
// Ownership: since round 3, session control lives server-side. `start` issues a
// control token exactly once; the daemon stores only a hash and never repeats
// it. This client keeps the token it was issued in a SessionCredential and
// injects it into every mutating request (act/cancel/pause/resume/takeover/
// release/stop) — a session id alone grants nothing.
//
// Timeouts: a timed-out `computer.act` is not simply abandoned. The client
// sends a precise `computer.cancel` (same connection, same request id, with the
// control token) and waits up to `cancelGracePeriodMs` for the daemon's
// acknowledgement; the resulting RequestTimeoutError reports whether the
// runtime actually confirmed the cancellation.

import { createConnection, type Socket } from "node:net";
import { homedir } from "node:os";
import { join } from "node:path";

import { AbortError, ComputerUseError, RequestTimeoutError, TransportError } from "./errors.js";
import {
  PROTOCOL_VERSION,
  type ActParams,
  type ActResult,
  type ApplicationInfo,
  type CancelParams,
  type CancelResult,
  type ClientInfo,
  type DesktopLayout,
  type DisplayInfo,
  type ExistingSessionPolicy,
  type Health,
  type InspectParams,
  type InspectResult,
  type ObserveParams,
  type ObserveResult,
  type PermissionStatus,
  type PointerInfo,
  type RuntimeVersion,
  type SessionAction,
  type SessionCredential,
  type SessionParams,
  type SessionResult,
  type TraceEntry,
  type TraceExport,
  type TraceList,
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
   * How long (ms) the client waits for the daemon to acknowledge the
   * `computer.cancel` it sends when a request times out, before giving up.
   * Clamped to 500–1500ms (default 1000). The timeout error reports
   * `runtimeCancellationConfirmed: false` when no acknowledgement arrived.
   */
  cancelGracePeriodMs?: number;
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
  signal?: AbortSignal;
  onAbort?: () => void;
}

/**
 * A connected client to the computer-use daemon. One connection supports any
 * number of concurrent requests (matched by JSON-RPC id).
 */
export class ComputerUseClient {
  readonly socketPath: string;
  readonly defaultTimeoutMs: number;
  readonly cancelGracePeriodMs: number;
  /** Identity used when this client starts a session. */
  readonly clientInfo: ClientInfo;

  private socket: Socket | null = null;
  private buffer = "";
  private pending = new Map<number, PendingRequest>();
  private nextId = 1;
  private closed = false;
  /** Single-flight guard: concurrent ensureSession() calls share one status→start. */
  private ensureSessionPromise: Promise<SessionResult> | null = null;
  /**
   * The credential this client holds for the session it started (or attached
   * to with a token). The control token never leaves this object except inside
   * the params of the mutating requests that require it.
   */
  private sessionCredential: SessionCredential | null = null;

  constructor(opts: ClientOptions = {}) {
    this.socketPath = opts.socketPath ?? defaultSocketPath();
    this.defaultTimeoutMs = opts.timeoutMs ?? 30_000;
    this.cancelGracePeriodMs = Math.min(1500, Math.max(500, opts.cancelGracePeriodMs ?? 1000));
    this.clientInfo = opts.clientInfo ?? {
      client_id: "sdk",
      client_name: "TypeScript SDK",
      client_instance_id: `${process.pid}-${Math.random().toString(36).slice(2, 10)}`,
    };
  }

  // -------------------------------------------------------------------------
  // Connection lifecycle
  // -------------------------------------------------------------------------

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
        // Round 3 is a breaking protocol change: refuse to talk to a daemon
        // that does not report protocol_version 2 (pre-ownership daemons).
        this.verifyDaemonProtocol(timeoutMs).then(
          () => resolve(this),
          (err: unknown) => {
            this.socket?.destroy();
            this.socket = null;
            this.closed = true;
            reject(err instanceof Error ? err : new TransportError(String(err)));
          },
        );
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

  /**
   * Check the daemon's wire protocol version after connecting. A daemon that
   * predates round 3 does not report `protocol_version`; either way, mismatch
   * means this SDK must not issue requests against it.
   */
  private async verifyDaemonProtocol(timeoutMs: number): Promise<void> {
    const v = await this.request<RuntimeVersion>(
      "runtime.version",
      { protocol_version: PROTOCOL_VERSION },
      { timeoutMs },
    );
    if (typeof v?.protocol_version !== "number") {
      throw new ComputerUseError(
        -32023,
        `daemon does not report a protocol version (pre-ownership daemon); this SDK requires v${PROTOCOL_VERSION}`,
        {
          code: "PROTOCOL_VERSION_MISMATCH",
          expected: PROTOCOL_VERSION,
        },
      );
    }
    if (v.protocol_version !== PROTOCOL_VERSION) {
      throw new ComputerUseError(
        -32023,
        `protocol version mismatch: daemon is v${v.protocol_version}, SDK is v${PROTOCOL_VERSION}`,
        {
          code: "PROTOCOL_VERSION_MISMATCH",
          expected: PROTOCOL_VERSION,
          got: v.protocol_version,
        },
      );
    }
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
      const onAbort = () => {
        // settlePending removes this listener, so this runs at most once.
        const pending = this.settlePending(id);
        if (!pending) return; // already settled by timeout/response — no double reject
        // Fire-and-forget precise cancel so a long wait/stabilizer stops
        // server-side too. The daemon's late response for the aborted request
        // is dropped: the pending entry is already gone.
        if (method !== "computer.cancel") this.notifyCancel(method, params, id);
        pending.reject(new AbortError(`request ${method} aborted by caller`));
      };
      const timer = setTimeout(() => {
        const pending = this.settlePending(id);
        if (!pending) return; // already settled (e.g. abort fired first)
        // The full cancel chain: a timed-out act tells the daemon to cancel the
        // in-flight batch and waits for the acknowledgement before reporting
        // the timeout — the SDK never claims the runtime stopped without proof.
        void this.handleRequestTimeout(method, params, id, timerMs, pending);
      }, timerMs);
      timer.unref();

      opts.signal?.addEventListener("abort", onAbort, { once: true });
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, timer, signal: opts.signal, onAbort });

      this.socket!.write(`${JSON.stringify(payload)}\n`, (err) => {
        if (err) {
          const pending = this.settlePending(id);
          if (pending) {
            pending.reject(new TransportError(`failed to write to daemon: ${err.message}`));
          }
        }
      });
    });
  }

  /**
   * Local timeout handler for a request whose deadline expired.
   *
   * Only `computer.act` carries a long-running batch worth cancelling
   * server-side; other methods time out as plain TransportErrors (the daemon
   * rejects the late response, which we drop). For `computer.act` the client
   * sends a precise cancel — same connection, the act's JSON-RPC id, with the
   * session's control token — and waits up to `cancelGracePeriodMs` for the
   * daemon's acknowledgement.
   */
  private async handleRequestTimeout(
    method: string,
    params: unknown,
    id: number,
    timerMs: number,
    pending: PendingRequest,
  ): Promise<void> {
    let runtimeCancellationConfirmed = false;
    if (method === "computer.act") {
      const sid = (params as { session_id?: unknown })?.session_id;
      if (typeof sid === "string" && sid) {
        const p = params as { control_token?: unknown };
        const token =
          typeof p.control_token === "string" ? p.control_token : this.tokenFor(sid);
        if (token) {
          try {
            const res = await this.request<CancelResult>(
              "computer.cancel",
              { session_id: sid, control_token: token, request_id: id },
              { timeoutMs: this.cancelGracePeriodMs },
            );
            runtimeCancellationConfirmed = res.cancelled === true;
          } catch {
            // No acknowledgement (daemon unreachable or cancel refused) —
            // report the timeout without claiming the runtime stopped.
            runtimeCancellationConfirmed = false;
          }
        }
      }
    }
    pending.reject(
      new RequestTimeoutError(
        `request ${method} timed out after ${timerMs}ms`,
        runtimeCancellationConfirmed,
      ),
    );
  }

  /**
   * Remove a pending request and clean up its timer and abort listener.
   * Returns `null` when the entry was already settled — callers must not
   * settle twice, and must not send a redundant cancel in that case.
   */
  private settlePending(id: number): PendingRequest | null {
    const pending = this.pending.get(id);
    if (!pending) return null;
    this.pending.delete(id);
    clearTimeout(pending.timer);
    if (pending.signal && pending.onAbort) {
      pending.signal.removeEventListener("abort", pending.onAbort);
    }
    return pending;
  }

  // -------------------------------------------------------------------------
  // Session credential
  // -------------------------------------------------------------------------

  /**
   * The credential this client holds for the session it started (or attached
   * to with an explicit token), or `null` when it has none.
   */
  getSessionCredential(): SessionCredential | null {
    return this.sessionCredential;
  }

  /** Replace the held credential (e.g. after a fresh `start`). */
  setSessionCredential(cred: SessionCredential | null): void {
    this.sessionCredential = cred;
  }

  /** Drop the held credential (e.g. the session was stopped or vanished). */
  clearSessionCredential(): void {
    this.sessionCredential = null;
  }

  /** The control token held for `sessionId`, if this client owns it. */
  private tokenFor(sessionId: string): string | undefined {
    if (this.sessionCredential && this.sessionCredential.sessionId === sessionId) {
      return this.sessionCredential.controlToken;
    }
    return undefined;
  }

  // -------------------------------------------------------------------------
  // Runtime introspection
  // -------------------------------------------------------------------------

  health(): Promise<Health> {
    return this.request<Health>("runtime.health");
  }

  version(): Promise<RuntimeVersion> {
    return this.request<RuntimeVersion>("runtime.version", {
      protocol_version: PROTOCOL_VERSION,
    });
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

  async session(
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
    // Mutating actions need the session's control token; `start` issues it and
    // `status` is read-only, so neither ever carries one. Only inject when the
    // client actually holds the token for this session — never override an
    // explicit token the caller provided.
    if (action !== "start" && action !== "status" && p.session_id && !p.control_token) {
      const token = this.tokenFor(p.session_id);
      if (token) p.control_token = token;
    }
    const result = await this.request<SessionResult>("computer.session", p, options ?? {});
    // Hold the capability the daemon just issued — it appears exactly once, in
    // the start response, and every later mutating call needs it. A successful
    // stop kills the token server-side, so the credential goes with it.
    if (action === "start" && result.control_token) {
      this.setSessionCredential({
        sessionId: result.session_id,
        controlToken: result.control_token,
        ownerClientId: result.owner_client_id,
        ownerInstanceId: result.owner_instance_id,
      });
    } else if (action === "stop" && this.sessionCredential?.sessionId === result.session_id) {
      this.clearSessionCredential();
    }
    return result;
  }

  /**
   * Ensure an active session exists: resolves the current one, or starts a new
   * one. Returns the resulting SessionResult.
   *
   * Single-flight: concurrent callers share one resolution, so two simultaneous
   * observe calls can never start two competing sessions.
   *
   * Ownership flow: if this client holds a credential it first confirms the
   * session still exists (a stopped session — or a daemon restart — invalidates
   * the token, so the credential is dropped and the flow starts fresh). Without
   * a usable credential, an active session that this client does not own is
   * handled per `policy`:
   *
   * - `"reject"` (default): the start attempt is refused and the daemon's
   *   CONTROL_LOCKED error (with the owner's identity, never a token) surfaces.
   * - `"read_only"`: the session's status is returned; the client must not act
   *   on it (it holds no token).
   * - `"attach_with_token"`: `attachControlToken` is the caller-supplied
   *   capability for the existing session, and becomes this client's
   *   credential, so later mutating requests are authorized.
   *
   * When no session exists, a new one is started and its freshly-issued token
   * is stored in the credential.
   */
  ensureSession(
    displayId?: string,
    options?: RequestOptions,
    clientInfo?: ClientInfo,
    policy: ExistingSessionPolicy = "reject",
    attachControlToken?: string,
  ): Promise<SessionResult> {
    if (!this.ensureSessionPromise) {
      this.ensureSessionPromise = this.ensureSessionInner(
        displayId,
        options,
        clientInfo,
        policy,
        attachControlToken,
      ).finally(() => {
        this.ensureSessionPromise = null;
      });
    }
    return this.ensureSessionPromise;
  }

  private async ensureSessionInner(
    displayId?: string,
    options?: RequestOptions,
    clientInfo?: ClientInfo,
    policy: ExistingSessionPolicy = "reject",
    attachControlToken?: string,
  ): Promise<SessionResult> {
    // 1. A credential is only usable while its session still exists.
    if (this.sessionCredential) {
      try {
        return await this.session(
          "status",
          { session_id: this.sessionCredential.sessionId },
          options,
        );
      } catch (err) {
        if (err instanceof ComputerUseError && err.code === "SESSION_NOT_FOUND") {
          // The session is gone (stopped, or the daemon restarted) — the token
          // died with it. Drop the credential and start fresh.
          this.clearSessionCredential();
        } else {
          throw err;
        }
      }
    }

    // 2. No usable credential: is there an active session we do not own?
    let active: SessionResult | null = null;
    try {
      active = await this.session("status", {}, options);
    } catch (err) {
      if (!(err instanceof ComputerUseError && err.code === "SESSION_NOT_FOUND")) {
        throw err;
      }
    }

    if (active) {
      if (policy === "attach_with_token") {
        if (!attachControlToken) {
          throw new ComputerUseError(-32602, "policy attach_with_token requires attachControlToken", {
            code: "INVALID_PARAMS",
          });
        }
        // The caller provided the capability; from now on this client acts as
        // an authorized operator of the existing session.
        this.setSessionCredential({
          sessionId: active.session_id,
          controlToken: attachControlToken,
          ownerClientId: active.owner_client_id,
          ownerInstanceId: active.owner_instance_id,
        });
        return active;
      }
      if (policy === "read_only") {
        // Return the status; the caller must not act — it holds no token.
        return active;
      }
      // "reject" (default): refuse to attach. Let the daemon answer the start
      // attempt — its CONTROL_LOCKED carries the real holder and the owner's
      // identity (never a token). If the other session ended in the meantime,
      // the start succeeds and this client owns the fresh session.
      return this.session(
        "start",
        { display_id: displayId, ...(clientInfo ? { ...clientInfo } : {}) },
        options,
      );
    }

    // 3. No active session: start one and hold its token.
    const started = await this.session(
      "start",
      { display_id: displayId, ...(clientInfo ? { ...clientInfo } : {}) },
      options,
    );
    if (started.control_token) {
      this.setSessionCredential({
        sessionId: started.session_id,
        controlToken: started.control_token,
        ownerClientId: started.owner_client_id,
        ownerInstanceId: started.owner_instance_id,
      });
    }
    return started;
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
    const p: ActParams = { ...params };
    // The daemon refuses a batch before executing anything without the token;
    // inject the one this client holds for the session (never override an
    // explicit token).
    if (!p.control_token) {
      const token = this.tokenFor(p.session_id);
      if (token) p.control_token = token;
    }
    return this.request<ActResult>("computer.act", p, options ?? {});
  }

  inspect(params: InspectParams, options?: RequestOptions): Promise<InspectResult> {
    return this.request<InspectResult>("computer.inspect", params, options ?? {});
  }

  cancel(params: CancelParams, options?: RequestOptions): Promise<CancelResult> {
    const p: CancelParams = { ...params };
    if (!p.control_token) {
      const token = this.tokenFor(p.session_id);
      if (token) p.control_token = token;
    }
    return this.request<CancelResult>("computer.cancel", p, options ?? {});
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
   * Fire-and-forget precise `computer.cancel` for a request that was aborted
   * locally. `request_id` pins it to this connection's request, so an abort in
   * one client can never cancel another client's request with the same id. The
   * control token is included when known; without it the daemon refuses the
   * cancel — which is correct, an abort is not a license to cancel others.
   */
  private notifyCancel(method: string, params: unknown, requestId: number): void {
    if (!this.socket || this.closed) return;
    const sid = (params as { session_id?: unknown } | undefined)?.session_id;
    if (typeof sid !== "string" || !sid) return;
    const p = params as { control_token?: unknown };
    const token = typeof p.control_token === "string" ? p.control_token : this.tokenFor(sid);
    const cancelParams: CancelParams = { session_id: sid, request_id: requestId };
    if (token) cancelParams.control_token = token;
    const payload = {
      jsonrpc: "2.0",
      id: this.nextId++,
      method: "computer.cancel",
      params: cancelParams,
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
    const pending = this.settlePending(id);
    if (!pending) return; // late response for a timed-out/aborted request
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
    for (const id of [...this.pending.keys()]) {
      const pending = this.settlePending(id);
      if (pending) pending.reject(err);
    }
  }
}

/**
 * Connect to the daemon and return a ready client.
 * Shorthand for `new ComputerUseClient(opts).connect()`.
 */
export function connect(opts: ClientOptions = {}): Promise<ComputerUseClient> {
  return new ComputerUseClient(opts).connect();
}
