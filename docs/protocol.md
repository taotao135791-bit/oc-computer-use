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

**Staleness is decided against the live screen**, not just by comparing id
numbers: a referenced frame is stale when the live desktop differs from it
(thumbnail diff over the configured threshold) or when the frontmost app
changed — even a slightly older `frame_id` whose content still matches the
screen is actionable. That is intentional: it rejects acting on outdated
reality, not merely on an old id.

| JSON-RPC code | Machine code | Meaning |
|---|---|---|
| -32700 | `PARSE_ERROR` | request was not valid JSON |
| -32601 | `METHOD_NOT_FOUND` | unknown method |
| -32602 | `INVALID_PARAMS` | missing/wrong params (e.g. observe without `session_id`) |
| -32000 | `INTERNAL` | internal failure |
| -32003 | `STALE_FRAME` | acted on a non-current frame |
| -32005 | `PAUSED` | session is paused |
| -32006 | `USER_TAKEOVER` | user is driving the machine |
| -32009 | `SESSION_NOT_FOUND` | unknown session id |
| -32010 | `INVALID_SESSION_STATE` | session already stopped, or transition invalid |
| -32015 | `UNSUPPORTED` | feature not supported (e.g. on this macOS) |
| — | `OUT_OF_BOUNDS` | coordinate outside the display |
| — | `CONTROL_LOCKED` | control lock held elsewhere |
| — | `CANCELLED` | action cancelled (computer.cancel) |
| — | `DRIVER_ERROR` | bridge/driver failure (e.g. Screen Recording permission missing) |
| — | `PERMISSION` | macOS permission missing |
| — | `CONFIRMATION_REQUIRED` | confirmation gating enabled |

## Methods

### Runtime introspection

| Method | Params | Result highlights |
|---|---|---|
| `runtime.health` | — | `version`, `ready`, `permissions`, `active_sessions`, `uptime_secs`, `frame_cache` |
| `runtime.version` | — | `name`, `version` |
| `runtime.permissions` | — | `screen_recording`, `accessibility` + guidance |
| `runtime.displays` | — | array of `{id, name, bounds, pixel_width, pixel_height, scale_factor, is_main}` |
| `runtime.desktop_layout` | — | `primary_id`, `displays` |
| `runtime.pointer` | — | `location: {x, y}` |
| `runtime.active_application` | — | `bundle_id`, `name`, optional `window_title` |
| `runtime.shutdown` | — | `{status: "shutting_down"}` |

### Sessions

`computer.session` — params `{action, session_id?, display_id?}`.

Actions: `start`, `status`, `pause`, `resume`, `takeover`, `release`, `stop`.
Result: `{session_id, state, paused, user_takeover, lock_held, display_id,
created_at, last_action_at, current_frame_id, trace_dir}`.

`computer.cancel` — params `{session_id}`. Cancels the in-flight action
(if any) and returns `{cancelled, session_id}`.

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

Result: `{session_id, frame_id, action_results: [{index, status,
duration_ms, error?}]}`.

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
