# @computer-use/sdk

TypeScript client for the computer-use daemon. Zero runtime dependencies.

```ts
import { connect } from "@computer-use/sdk";

const client = await connect();
const session = await client.ensureSession();
const frame = await client.observe({ include_image: true });
const result = await client.act({
  session_id: session.session_id,
  frame_id: frame.frame_id,
  actions: [{ type: "click", x: 500, y: 400, coordinate_space: "normalized_1000", button: "left" }],
});
```

Errors: daemon errors throw `ComputerUseError` (`.code` is `"STALE_FRAME"`,
`"PAUSED"`, `"OUT_OF_BOUNDS"`, …); connection failures throw `TransportError`.
