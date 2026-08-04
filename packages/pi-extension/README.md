# @computer-use/pi-extension

A [Pi](https://pi.do.computer) (pi-coding-agent) extension that exposes the
computer-use runtime to agents as **tools with real screenshot image
content blocks** plus slash commands for session control. Built on Pi's
official `@earendil-works/pi-coding-agent` Extension API (default-export
factory).

## Install into Pi

```bash
pnpm add @computer-use/pi-extension
```

Register it in your Pi config (see [Pi's extension docs](https://pi.do.computer/docs/extensions)):

```ts
// pi.config.ts
import computerUseExtension from "@computer-use/pi-extension";

export default computerUseExtension;
```

The extension talks to the daemon at `~/.computer-use/runtime.sock`
(override with `COMPUTER_USE_SOCKET`). Sessions are started lazily on first
tool use.

## Tools

| Tool | Purpose |
|---|---|
| `computer_session` | start / status / pause / resume / takeover / release / stop |
| `computer_observe` | screenshot → text metadata + a **real image content block** (base64 + mimeType) |
| `computer_act` | click / double_click / move / type / key / scroll / drag / wait on a frame; returns per-action results + the post-batch screenshot as an image block |
| `computer_inspect` | crop a region of a frame → image block + coordinate mapping text |

All runtime safety (stale frames, pause, takeover, bounds, control lock) is
enforced server-side; errors surface with the runtime's error codes.

## Commands

| Command | Purpose |
|---|---|
| `/computer-status` | daemon health + real session state |
| `/computer-start` | start a session |
| `/computer-pause` / `/computer-resume` | pause / resume |
| `/computer-takeover` / `/computer-release` | human takeover / hand control back |
| `/computer-stop` | stop the session |
| `/computer-observe` | save the current frame as PNG into the working directory |

## Cancellation & lifecycle

- Tool executions honor Pi's `AbortSignal`: a pre-aborted call returns
  `Cancelled` immediately; an in-flight call propagates the abort to the
  daemon request.
- The extension listens for Pi's `session_shutdown` event and stops the
  active session and closes its connection on quit / reload / session
  switch.

## Tests

```bash
pnpm test   # 9 tests (fake daemon, drive the registered tools directly)
```
