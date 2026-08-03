# @computer-use/pi-extension

A Pi extension that turns the computer-use runtime into tools **plus** a
ready-made driver loop (`runDriverLoop`) for agents that don't want to wire
tools by hand.

## Install into Pi

```bash
pnpm add @computer-use/pi-extension
```

Then register it with your Pi config (see
[pi's extension docs](https://pi.do.computer/docs/extensions)):

```js
import { ComputerUseExtension } from "@computer-use/pi-extension";

export const extensions = [new ComputerUseExtension()];
```

`ComputerUseExtension` exposes `toolSchemas()` (the four computer tools +
trace tools), `ensureSession()`, `handleTool(name, args)` and
`runDriverLoop(model, { maxSteps, systemPrompt? })`.

## Driver Mode

```ts
const ext = new ComputerUseExtension();
await ext.ensureSession();

await ext.runDriverLoop(
  {
    decide: async (ctx) => {
      // ctx: { frame, history } — frame has image (base64), size, app;
      // history is the list of past {action, result} records.
      // Call your model of choice with ctx, then return:
      return { kind: "act", action: { type: "click", x: 500, y: 400 } };
      // or { kind: "done" } when finished, { kind: "error", message } on failure.
    },
  },
  { maxSteps: 10, systemPrompt: undefined }
);
```

`DRIVER_SYSTEM_PROMPT` is exported as a sensible default system prompt for
models that take one.

Safety is enforced by the runtime, not the loop: STALE_FRAME results are
retried with a fresh observe; PAUSED / USER_TAKEOVER / INVALID_SESSION_STATE
errors surface as tool errors for the model to react to.

## Without the loop

If you prefer wiring Pi tools yourself, use `ext.toolSchemas()` for schema and
`ext.handleTool()` for execution — both accept and return plain JSON.

## Tests

```bash
pnpm test   # 8 tests incl. a driver loop and stale-frame retry (fake daemon)
```
