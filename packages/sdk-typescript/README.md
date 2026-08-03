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

`observe()` auto-resolves the session: if none is active it starts one first.

## Errors

All failures throw `ComputerUseError` with `.code` (machine code like
`STALE_FRAME`), `.jsonrpcCode` and `.data`. See
[`src/errors.ts`](src/errors.ts) for the code table.

## Tests

```bash
pnpm test    # node --test, hermetic against a fake daemon
```
