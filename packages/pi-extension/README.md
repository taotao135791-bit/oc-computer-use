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
(override with `COMPUTER_USE_SOCKET`).

## Sessions: auto-create and ownership

- **Auto-create on first use.** The first tool call resolves the active
  session (single-flight) and only starts one when the daemon reports
  `SESSION_NOT_FOUND` — so a freshly installed setup works with zero
  configuration. The started session records the extension's identity
  (`pi-extension` / `Pi` / `pi-<pid>-<rand>`), making it the **owner**.
- **Ownership.** The extension tracks the session it created (`ownsSession`).
  Only sessions it owns are stopped on `session_shutdown` — a session another
  client created is left running. When a session owned by another client is
  already active, the behavior is controlled by
  `COMPUTER_USE_EXISTING_SESSION_POLICY`:
  - `reject` (default): tools fail with `CONTROL_LOCKED` ("Another client
    owns the active computer-use session.") — the extension never takes over.
  - `attach`: tools observe/act on the foreign session, but can never stop
    it. There is deliberately no `start_new` policy — the runtime allows only
    one active session.

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
| `/computer-observe` | save the current frame to a temp-file screenshot (see below) |

## Screenshot saving (`/computer-observe`)

Saved screenshots never touch the repo, project, or working directory:

- Location: the system temp dir (`os.tmpdir()`), named
  `oc-computer-use-<sessionId>-<frameId>.<ext>`.
- Extension by MIME: `image/jpeg` → `.jpg`, `image/png` → `.png`
  (unknown MIME types are rejected, not guessed).
- Permissions: `0600` — only your user can read them.
- Cleanup: a temp-image manager removes screenshots after an age timeout, on
  session stop, and on extension exit. Screenshots the trace machinery needs
  are never deleted. The full screenshot base64 is never logged or recorded.

## Cancellation & lifecycle

- Tool executions honor Pi's `AbortSignal`: a pre-aborted call returns
  `Cancelled` immediately; an in-flight call propagates the abort to the
  daemon request (`computer.cancel` on the JSON-RPC connection), which
  cancels the running wait/action batch — a long `wait` returns
  `action[0]: cancelled` almost immediately.
- The extension listens for Pi's `session_shutdown` event; on quit / reload /
  session switch it stops the session **only if it owns it** (see ownership
  above) and closes its connection.

## Tests

```bash
pnpm test   # 14 tests (stateful fake daemon, drive the registered tools directly)
```
