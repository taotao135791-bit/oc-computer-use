# @computer-use/mcp-server

MCP (Model Context Protocol) server exposing the computer-use runtime as 7
tools. Images are returned as MCP **image content blocks** so vision-capable
clients see the screenshot directly.

**Sessions are created on first use.** The first `computer_observe` /
`computer_act` auto-starts a session when none is active; the daemon records
this server as the session owner (`mcp-server` / "Computer Use MCP", with a
per-process instance id), and the session can only be stopped through this
server (or its owner). A `computer_session status` with no active session
returns `SESSION_NOT_FOUND` — the error arrives as an `isError` content
block, never as a thrown exception.

## Run

```bash
# socket autodetected from COMPUTER_USE_SOCKET (default ~/.computer-use/runtime.sock)
computer-use-mcp

# with an explicit socket
COMPUTER_USE_SOCKET=/tmp/other.sock computer-use-mcp
```

Requires the daemon to be running (`cu daemon start`).

## Tools

| Tool | Params | Returns |
|---|---|---|
| `computer_session` | `action` (start/status/pause/resume/takeover/release/stop), `session_id?`, `display_id?` | session state (incl. owner) |
| `computer_observe` | `session_id?`, `include_image?`, `include_cursor?`, `max_width?`, `image_format?`, `display_id?` | image content block + frame metadata (`frame_id`, size, app) |
| `computer_act` | `session_id?`, `frame_id?`, `actions` (structured array), `wait_policy?`, `return_screenshot?` | per-action results + post-batch screenshot |
| `computer_inspect` | `session_id?`, `frame_id?`, `region` (structured object), `scale?` | cropped image + global-origin mapping |
| `computer_cancel` | `session_id?` | cancellation ack |
| `trace_list` | — | trace summary table |
| `trace_get` | `session_id` | parsed trace entries |

**No JSON-string-inside-JSON.** `computer_act` takes `actions` as a
structured array of objects discriminated on `type` (click / double_click /
move / type / key / scroll / drag / wait), each with its real field list;
`computer_inspect`'s `region` is a structured object `{x, y, width, height,
coordinate_space}`. The MCP input schema is generated from these zod
schemas, so a model sees the exact fields. Example of one action:

```json
{ "type": "click", "x": 500, "y": 400, "button": "left", "coordinate_space": "normalized_1000" }
```

Tool errors (e.g. `STALE_FRAME`, `CONTROL_LOCKED`, `SESSION_NOT_FOUND`)
arrive as a successful RPC with `isError: true` and the error text in the
content — the server never throws across the wire.

## MCP clients

```bash
# claude
claude mcp add computer-use -- npx computer-use-mcp

# codex / generic MCP clients
npx @computer-use/mcp-server
```

## Programmatic use

```ts
import { createComputerUseServer } from "@computer-use/mcp-server";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
// createComputerUseServer() returns a configured McpServer; pass your own
// transport (stdio is default).
```

## Tests

```bash
pnpm test   # 10 tests: spawns the real server, drives it with an McpClient
            # over stdio (newline-delimited JSON framing)
```

## Real-environment acceptance

`node scripts/opencode-mcp-acceptance.mjs` (repo root) spawns the exact
binary OpenCode launches and drives it against the **real** daemon and
screen — first-observe auto-create (owned by `mcp-server`), real image
content blocks, stale-frame rejection after a window switch, a 30s wait
cancelled in under a second, and session stop. See
[docs/acceptance-manual.md](../../docs/acceptance-manual.md).
