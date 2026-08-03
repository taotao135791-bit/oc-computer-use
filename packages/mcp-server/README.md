# @computer-use/mcp-server

MCP (Model Context Protocol) server exposing the computer-use runtime as 7
tools. Images are returned as MCP **image content blocks** so vision-capable
clients see the screenshot directly.

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
| `computer_session` | `action` (start/status/pause/resume/takeover/release/stop), `session_id?` | session state |
| `computer_observe` | `session_id?`, `include_image?`, `display_id?` | image content block + frame metadata (`frame_id`, size, app) |
| `computer_act` | `session_id?`, `frame_id?`, `actions` (JSON string) | per-action results |
| `computer_inspect` | `session_id?`, `frame_id?`, `region` (JSON string), `scale?` | cropped image + global-origin mapping |
| `computer_cancel` | `session_id?` | cancellation ack |
| `trace_list` | — | trace summary table |
| `trace_get` | `session_id` | parsed trace entries |

`computer_act` takes `actions` as a JSON string, e.g.
`'[{"type":"click","x":500,"y":400,"button":"left","coordinate_space":"normalized_1000"}]'`.

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
pnpm test   # 7 end-to-end tests: spawns the real server, drives it with an
            # McpClient over stdio (newline-delimited JSON framing)
```
