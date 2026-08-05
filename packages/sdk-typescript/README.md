# @computer-use/sdk

TypeScript client for the computer-use runtime. Talks JSON-RPC 2.0
(newline-delimited JSON) to the daemon over the Unix socket
`~/.computer-use/runtime.sock`. Zero runtime dependencies.

## Usage

```ts
import { ComputerUseClient } from "@computer-use/sdk";

const client = new ComputerUseClient({ socketPath: process.env.COMPUTER_USE_SOCKET });
await client.connect();

const health = await client.health();
await client.ensureSession();            // start a session if none is active

const frame = await client.observe({ include_image: true });   // frame_id + base64 image

await client.act({
  frame_id: frame.frame_id,
  actions: [
    { type: "click", x: 500, y: 400, button: "left", coordinate_space: "normalized_1000" },
    { type: "type", text: "hello world" },                     // redacted in traces
  ],
});

const crop = await client.inspect({ region: { x: 100, y: 100, width: 200, height: 200, coordinate_space: "image_pixels" } });
await client.session("pause");
await client.session("resume");
await client.session("stop");

const traces = await client.traceList();
const entries = await client.traceGet("s_abc");
client.close();
```

## API

- **Connection**: `connect()`, `close()` — automatic reconnection on next call.
- **Runtime**: `health()`, `version()`, `permissions()`, `displays()`,
  `desktopLayout()`, `pointer()`, `activeApplication()`, `shutdown()`.
- **Session**: `ensureSession()`, `session(action, params?)` (start / status /
  pause / resume / takeover / release / stop).
- **Computer**: `observe(params)`, `act(params)`, `inspect(params)`, `cancel(params)`.
- **Traces**: `traceList()`, `traceGet(sessionId)`, `traceExport(sessionId, dest)`, `traceReplay(sessionId)`.

`observe()` auto-resolves the session: `ensureSession()` queries
`session("status")` and starts one **only** when the daemon reports
`SESSION_NOT_FOUND` (other errors are rethrown); concurrent callers share one
single-flight resolution, so exactly one session is created. `connect({clientInfo})`
sets the identity sent with `session start` (default `sdk` / "TypeScript
SDK") — the daemon records it as the session owner, and the owner fields
(`owner_client_id`, `owner_client_name`, `owner_instance_id`) are returned
in every session result.

## Session credentials (capability)

The daemon issues a session's **two capability tokens exactly once**, in the
`start` response: an `observationToken` (sensitive reads: observe, inspect,
status, trace) and a `controlToken` (mutating operations; also valid for
reads). **Knowing a session ID grants neither observation nor control
permission** — the daemon verifies token hashes before any side effect.

The SDK holds the tokens in an in-memory `SessionCredential`:

```ts
interface SessionCredential {
  sessionId: string;
  observationToken: string;
  controlToken?: string;   // absent for read_only adoptions
  ownerClientId?: string;  // identity the owner reported at start
  ownerInstanceId?: string;
  access: "read_only" | "control";
}
```

It automatically injects the right token into the calls it owns — `observe`,
`inspect`, `status`, `trace*` carry the observation token; `act`, `cancel`,
`pause`, `resume`, `takeover`, `release`, `stop` carry the control token (an
explicit `*_token` in the params always wins). `stop` clears the credential.
`close()` only closes the socket — it never stops a session.

**Credentials stay in memory only.** The SDK never persists tokens to disk;
if a process restarts it has no tokens (sessions started before the restart
are simply owned by someone else). Only the CLI (`cu`) persists session
credentials, to files with mode `0600` under `~/.local/state/oc-computer-use/`.

**Existing sessions: default `reject`.** When `ensureSession` finds an active
session it does not own, it must not silently attach. The `existingPolicy`
argument controls this:

- `reject` (default): a `start` probe surfaces the daemon's `CONTROL_LOCKED`
  (with the owner's non-secret identity in `data`) — nothing is disturbed.
- `read_only`: returns the foreign session for observe-only use; no
  credential is held, so mutating calls are refused by the daemon
  (`CONTROL_TOKEN_REQUIRED`).
- `attach_with_token`: adopt the session only with a token the caller
  supplies (e.g. from its own credential store); the adopted credential is
  injected into later mutating calls.

## Timeouts & cancellation

A request that exceeds its timeout is rejected with `RequestTimeoutError`
(code `REQUEST_TIMEOUT`, -32022). The SDK then sends a **precise**
`computer.cancel` for that exact request — keyed by connection + request id
+ session + control token, so client A can never cancel client B's request —
and waits `cancelGracePeriodMs` (default 1000 ms, clamped 500–1500 ms) for
the daemon's acknowledgement. `err.runtimeCancellationConfirmed` is `true`
**only** when the daemon acknowledged the cancel: the SDK never claims the
runtime stopped without proof. Pending requests are settled (`settlePending`)
on timeout, abort, close, and daemon death — no listener or pending entry
outlives its request. `AbortSignal` maps to the same cancel path (a cancel
is only sent when a session id and token are known).

## Errors

All failures throw `ComputerUseError` with `.code` (machine code like
`STALE_FRAME`), `.jsonrpcCode` and `.data`. See
[`src/errors.ts`](src/errors.ts) for the code table.

## Tests

```bash
pnpm test    # node --test, hermetic against a fake daemon
```
