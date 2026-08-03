# @computer-use/opencode-adapter

An [OpenCode](https://opencode.ai) plugin that exposes the computer-use
runtime as four tools: `computer_observe`, `computer_act`, `computer_inspect`,
`computer_session`.

## Install

```bash
pnpm add @computer-use/opencode-adapter
```

## Configure

```bash
opencode add plugin -- npm:@computer-use/opencode-adapter
```

or add to your OpenCode config (`config/opencode.config.json` — see the
example in this repo):

```json
{
  "plugins": {
    "computer-use": {
      "type": "npm",
      "name": "@computer-use/opencode-adapter"
    }
  }
}
```

The plugin default-exports `computerUsePlugin()` which the loader picks up
automatically. The tools speak to the daemon at
`~/.computer-use/runtime.sock` (override with `COMPUTER_USE_SOCKET`).

## Tools

| Tool | Purpose |
|---|---|
| `computer_observe` | screenshot → frame_id + image + metadata |
| `computer_act` | click/move/type/key/scroll/drag/wait on a frame |
| `computer_inspect` | crop a region of a stored frame |
| `computer_session` | session lifecycle |

All runtime safety (stale frames, pause, takeover, bounds) is enforced
server-side, so the adapter stays thin.

## Programmatic use

```ts
import { computerUsePlugin } from "@computer-use/opencode-adapter";

export default computerUsePlugin({ socketPath: process.env.COMPUTER_USE_SOCKET });
```

## Tests

```bash
pnpm test   # 6 tests (fake daemon)
```
