# Protocol

JSON-RPC 2.0 over a **Unix domain socket** at `~/.computer-use/runtime.sock`
(`COMPUTER_USE_HOME` overrides the directory). Requests and responses are
newline-delimited JSON — one object per line. The socket is created with mode
`0700`; a stale socket from a previous run is replaced at startup.

Example:

```
→ {"jsonrpc":"2.0","id":1,"method":"runtime.health","params":null}
← {"jsonrpc":"2.0","id":1,"result":{"version":"0.1.0","ready":true,...}}
```

## Error shape

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "error": {
    "code": -32003,
    "message": "STALE_FRAME",
    "data": {
      "code": "STALE_FRAME",
      "message": "frame frame_2 is stale (expected frame_3)",
      "detail": { "expected_frame_id": "frame_3", "given_frame_id": "frame_2" }
    }
  }
}
```

`error.code` is the JSON-RPC number, `error.message` is the stable machine
code, and `data` carries the same code plus a human message and detail.

**Staleness is decided against the live screen** in addition to id
comparisons: a referenced frame is stale when the live desktop differs from
it (thumbnail diff over the configured threshold), when the frontmost app
changed, or when it is too old. Two policies control how strictly the
referenced `frame_id` itself is checked (`COMPUTER_USE_STALE_POLICY`):

- `strict` (default): only the session's **current** frame is actionable;
  acting on any older `frame_id` is `STALE_FRAME` regardless of how similar
  the pixels are. The live-screen checks still run on top.
- `visual_match`: a slightly older `frame_id` whose content still matches the
  live screen is actionable (the live-screen checks decide).

| JSON-RPC code | Machine code | Meaning |
|---|---|---|
| -32700 | `PARSE_ERROR` | request was not valid JSON |
| -32601 | `METHOD_NOT_FOUND` | unknown method |
| -32602 | `INVALID_PARAMS` | missing/wrong params (e.g. observe without `session_id`) |
| -32000 | `INTERNAL` | internal failure |
| -32003 | `STALE_FRAME` | acted on a stale/out-of-date frame |
| -32004 | `CONTROL_LOCKED` | session already active and owned by another client (or control lock held elsewhere); `data` carries `holder` and the owner's non-secret identity |
| -32005 | `PAUSED` | session is paused |
| -32006 | `USER_TAKEOVER` | user is driving the machine |
| -32009 | `SESSION_NOT_FOUND` | no active session (status with no session → `"No active computer-use session exists."`) |
| -32010 | `INVALID_SESSION_STATE` | session already stopped, or transition invalid |
| -32012 | `CANCELLED` | action cancelled (computer.cancel / abort) |
| -32015 | `UNSUPPORTED` | feature not supported (e.g. on this macOS) |
| -32016 | `USER_TAKEOVER_ACTIVE` | `resume` while the user holds control — call `release` first |
| -32017 | `ACTION_TIMEOUT` | request exceeded the daemon deadline (batch still running) |
| -32018 | `CAPTURE_FAILED` | screen capture failed (driver/capture failure) |
| -32019 | `CONTROL_TOKEN_REQUIRED` | a mutating operation was attempted without the session's control token |
| -32020 | `INVALID_CONTROL_TOKEN` | a control token was presented but did not verify (deliberately non-descriptive) |
| -32021 | `SESSION_STOPPED` | a mutating operation targeted a session that is already stopped |
| -32022 | `REQUEST_TIMEOUT` | the client-side request deadline expired (reported by the SDK, see below) |
| -32023 | `PROTOCOL_VERSION_MISMATCH` | the client's `protocol_version` is incompatible with the daemon's |
| — | `OUT_OF_BOUNDS` | coordinate outside the display |
| — | `DRIVER_ERROR` | bridge/driver failure (e.g. Screen Recording permission missing) |
| — | `PERMISSION` | macOS permission missing |
| — | `CONFIRMATION_REQUIRED` | confirmation gating enabled |
| — | `TRACE_ERROR` | trace unavailable/failed (see trace modes below) |

### Trace modes

Trace recording policy is set with `COMPUTER_USE_TRACE_MODE`:

- `best_effort` (default): a trace write failure degrades the trace (the
  response's `trace` object reports `degraded: true` + warnings) but the
  operation succeeds.
- `required`: session start fails if a trace cannot be opened; an act batch
  fails if its trace entries cannot be written.
- `disabled`: no trace recorder is created; `computer.act` responses have no
  `trace` object.

`computer.act` responses include `trace: {mode, degraded, warnings}` when a
recorder exists, so callers can tell when a trace may be incomplete. `type`
text is always redacted to `{text_redacted, character_count}` unless the
daemon runs with `COMPUTER_USE_TRACE_DEV_MODE=1`.

## Methods

### Runtime introspection

| Method | Params | Result highlights |
|---|---|---|
| `runtime.health` | — | `version`, `ready`, `permissions`, `active_sessions`, `uptime_secs`, `frame_cache` |
| `runtime.version` | `{protocol_version}` | `name`, `version`, `protocol_version` — advertise your protocol version (`2`); a mismatch is `PROTOCOL_VERSION_MISMATCH` |
| `runtime.permissions` | — | `screen_recording`, `accessibility` + guidance |
| `runtime.displays` | — | array of `{id, name, bounds, pixel_width, pixel_height, scale_factor, is_main}` |
| `runtime.desktop_layout` | — | `primary_id`, `displays` |
| `runtime.pointer` | — | `location: {x, y}` |
| `runtime.active_application` | — | `bundle_id`, `name`, optional `window_title` |
| `runtime.shutdown` | — | `{status: "shutting_down"}` |

### Sessions

`computer.session` — params `{action, session_id?, display_id?, control_token?,
client_id?, client_name?, client_instance_id?}`.

Actions: `start`, `status`, `pause`, `resume`, `takeover`, `release`, `stop`.
Result: `{session_id, state, paused, user_takeover, lock_held, display_id,
created_at, last_action_at, current_frame_id, trace_dir, owner_client_id,
owner_client_name, owner_instance_id, control_token?}`.

#### Control token (capability)

The daemon issues a session's **control token exactly once**, in the `start`
response. It is a 256-bit random value (base64url), returned **only there**:
`status` and every other read-only call never repeat it. The daemon stores
only a SHA-256 hash and never logs, traces, or prints it. **Every mutating
operation — `pause`, `resume`, `takeover`, `release`, `stop`, `computer.act`,
`computer.cancel` — is refused without the token** (`CONTROL_TOKEN_REQUIRED`),
and a wrong token is refused (`INVALID_CONTROL_TOKEN`) with no side effects.
Read-only operations (`status`, `observe`, `inspect`, ...) need no token.

**Knowing a session ID does not grant control.** A client that knows a
session's id — through `status`, a trace, or another client's output — can
read it, but cannot pause, stop, or cancel it. The token is the capability;
the id is an address.

Token lifecycle:

- `pause` / `resume` / `takeover` / `release` never change the token.
- `stop` ends the session; the token dies with it (the daemon forgets the
  hash, and a later `stop` on the stopped session is `SESSION_STOPPED`).
- A daemon restart ends every in-memory session and invalidates every token;
  stored credentials must be treated as dead after a restart.

#### Session behavior

- **Auto-create on first use.** Clients that expect a session (SDK
  `ensureSession`, CLI, MCP, Pi extension) resolve the active session with
  `status` first and start one **only** when `status` fails with
  `SESSION_NOT_FOUND` — other errors are rethrown, never masked. The resolve
  is single-flight, so concurrent first calls start exactly one session.
- **Ownership.** `start` takes optional identity params
  `{client_id, client_name, client_instance_id}`; the daemon records them on
  the session and returns them on every `session_result` (`owner_*` fields).
  The owner identity is non-secret and appears in `status`; the control token
  is the secret that proves the right to mutate. The runtime itself allows
  only **one active session** — a second `start` while one is active fails
  with `CONTROL_LOCKED`, whose `data` carries the holder's session id and the
  owner's identity (never a token).
- **Existing sessions: default is `reject`.** When a client finds an active
  session it does not own, it must **not** silently attach: the SDK's
  `ensureSession` defaults to `reject` (the daemon's `CONTROL_LOCKED` surfaces
  to the caller), with `read_only` (observe-only, no token) and
  `attach_with_token` (the caller supplies the token, e.g. from its own
  credential store) as explicit opt-ins. No client auto-attaches in a way
  that would let it stop another client's session.
- **`status` with no session** returns `SESSION_NOT_FOUND` (code -32009)
  with `"No active computer-use session exists."` — this is the
  signal to create.

`computer.cancel` — params `{session_id, control_token, request_id?,
connection_id?}`. Cancellation is **precise** and token-verified: with
`request_id`, exactly the request with that JSON-RPC id on the *same
connection* is cancelled (the daemon keys in-flight batches by
`(connection_id, request_id)`, so client A can never cancel client B's
request even with an identical id); without it, the whole session's in-flight
batch is cancelled. Returns `{cancelled, session_id}`. The cancelled batch
stops at the next safe boundary (a long `wait` exits immediately) and its
response reports the already-executed actions as `success` and the rest as
`cancelled` — a cancelled `wait` is never reported as an internal error.
Clients may also abort over the connection: the SDK maps an `AbortSignal`
to a `computer.cancel` with the request's own id, so Pi/OpenCode
cancellation reaches the daemon through the same path.

#### Client-side timeouts

When an SDK request times out (`REQUEST_TIMEOUT`, -32022), the SDK sends a
precise `computer.cancel` for that request and waits a short grace period
(`cancelGracePeriodMs`, clamped 500–1500ms) for the daemon's acknowledgement.
The thrown `RequestTimeoutError` carries `runtimeCancellationConfirmed`:
`true` only when the daemon acknowledged the cancel — the SDK never claims
the runtime stopped without proof. Unconfirmed cancels are reported honestly
so the caller can re-check state instead of assuming.

### computer.observe

Params: `{session_id, display_id?, include_image?, max_width?, format?,
quality?}`.

Result:

```json
{
  "session_id": "s_abc",
  "frame_id": "frame_3",
  "width": 1440,
  "height": 900,
  "display_id": "1",
  "scale_factor": 2,
  "active_application": "Google Chrome",
  "active_window": "untitled.html — Chrome",
  "image_path": "/Users/you/.computer-use/frames/s_abc_3.jpg",
  "image_mime_type": "image/jpeg",
  "image_base64": "<base64, only when include_image>",
  "captured_at": "2026-08-03T05:00:00Z"
}
```

### computer.act

Params: `{session_id, frame_id?, actions: [ComputerAction]}`.

`ComputerAction` is a discriminated union on `"type"`:

```json
{"type": "click",  "x": 500, "y": 400, "button": "left",  "coordinate_space": "normalized_1000"}
{"type": "double_click", "x": 500, "y": 400, "button": "left"}
{"type": "move",   "x": 500, "y": 400}
{"type": "type",   "text": "hello", "text_input_method": "keyboard"}   // redacted in traces
{"type": "key",    "keys": ["cmd", "l"]}
{"type": "scroll", "delta_x": 0, "delta_y": -300, "x": 500, "y": 400}
{"type": "drag",   "from_x": 100, "from_y": 100, "to_x": 300, "to_y": 300}
{"type": "wait",   "duration_ms": 500}
```

Coordinates: `normalized_1000` (0–1000 across the display) or `image_pixels`
(the pixels of the frame returned by observe). All actions accept
`wait_policy`: `none` | `fixed` (with `fixed_wait_ms`) | `until_stable`.

Result: `{executed, action_results: [{index, status, duration_ms, error?}],
screen_changed, stable, next_frame_id?, screenshot?, stabilization?, trace?}`.
`stabilization` (when `wait_policy: "until_stable"`) is
`{outcome: "stable"|"timed_out", change_score, samples, elapsed_ms?}` —
`change_score` is the **last measured** thumbnail difference, never a
fabricated 0, so a timeout reports how close the screen actually got to
settling. `trace` is `{mode, degraded, warnings}` (see [Trace modes](#trace-modes)).

### computer.inspect

Params: `{session_id, frame_id?, region: {x, y, width, height,
coordinate_space}, scale?}`. Returns `{session_id, frame_id, width, height,
image_mime_type, image}` plus `mapping: {global_origin: [x, y]}` so the agent
can translate cropped pixels back to global coordinates.

### Traces

| Method | Params | Result |
|---|---|---|
| `trace.list` | — | `{traces: [{session_id, path, entries, bytes, started_at}]}` |
| `trace.get` | `{session_id}` | `{entries: [...]}` (parsed JSONL) |
| `trace.export` | `{session_id, dest}` | `{path, format, exported_at}` |
| `trace.replay` | `{session_id}` | replay summary |

## Reference clients

- **CLI**: `cu <command>` — the reference client; every subcommand supports
  `--json` and exits non-zero on failure.
- **TypeScript SDK**: `ComputerUseClient` (see
  [packages/sdk-typescript](../packages/sdk-typescript/README.md)) — handles
  connection, framing, out-of-order id matching, and auto-session.
- **MCP**: 7 tools exposing this surface (see
  [packages/mcp-server](../packages/mcp-server/README.md)).
