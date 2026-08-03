# Architecture

## Overview

```
                  JSON-RPC 2.0 (newline-delimited) over Unix domain socket
                  ~/.computer-use/runtime.sock  (mode 0700, current user only)

 agent (Pi / OpenCode / Codex / any model) ────────┐
        │                                          ▼
   TS SDK (ComputerUseClient)               +------------------+
   MCP Server (7 tools)          ─────────▶ │  cu-daemon       │
   cu CLI (cu observe …)                     │  jsonrpc dispatch│
   cu-inspector (HTTP dashboard)             │  request timeout │
                                            +------------------+
                                                     │
                                                     ▼
                                            +------------------+
                                            │  cu-runtime      │
                                            │  session state   │
                                            │  control lock    │
                                            │  action queue    │
                                            │  stale-frame     │
                                            │  stabilizer      │
                                            │  trace recorder  │
                                            +------------------+
                                                     │
                                                     ▼
                                            +------------------+
                                            │  cu-driver-macos │
                                            │  Bridge (Rust)   │
                                            │  └─ cubridge     │  Swift child process
                                            │     (SCK, AX,    │  (line-JSON over pipes)
                                            │      pasteboard) │
                                            +------------------+
                                                     │
                          ┌──────────────────────────┼───────────────────┐
                          ▼                          ▼                   ▼
                  ScreenCaptureKit            CGEvent FFI           NSPasteboard
                  (screenshots)               (mouse/keyboard)      (clipboard
                                                                      text input)
```

## Crates

| Crate | Responsibility |
|---|---|
| `cu-core` | shared types (`ComputerAction`, `Point`, `Region`, `WaitPolicy`, …), error codes, tuning knobs, path helpers, frame-id scheme |
| `cu-driver` | the `ComputerDriver` trait (observe/capture, mouse, keyboard, displays, clipboard, permissions) |
| `cu-driver-macos` | the macOS implementation; owns the Swift bridge lifecycle and the CoreGraphics FFI |
| `cu-runtime` | sessions, control lock, stale-frame policy, stabilizer, action execution with pre-checks, trace integration |
| `cu-trace` | JSONL trace recorder/storage, redaction, export/replay |
| `cu-daemon` | the JSON-RPC server: dispatch, per-request timeout, socket lifecycle |
| `cu-cli` | the `cu` binary: daemon management, session/observe/act, traces |

The daemon crate is **lib-only**; the binary lives in `cu-cli` so that the
integration tests can run the real daemon in-process (`cu_daemon::run`).

## Threading model

- **Daemon**: async (tokio). One task per client connection; requests are
  dispatched with a per-request deadline (`request_timeout_secs`, default 600s;
  session pause/stop/cancel are the fast cancellation path).
- **Bridge** (`cubridge`): a separate Swift process owned by the driver. Rust
  ⇄ Swift traffic is one JSON object per line on stdin/stdout. All bridge I/O
  is bounded: `try_request` waits at most 30s using `poll(2)` with a deadline,
  so a wedged bridge (e.g. a stuck ScreenCaptureKit call) produces a driver
  error instead of blocking the daemon's executor. If the bridge dies, the
  next request respawns it.
- **Driver actions** (mouse/keyboard via CGEvent FFI) run synchronously in the
  request handler; they are sub-millisecond.

## Session lifecycle

```
 start ─▶ active ── pause ──▶ paused ── resume ─▶ active
   │          │                    ▲
   │          └── user takeover ───┤  (any time)
   ▼                              ▼
 stopped ◀───── stop ────────────┘

 state machine enforced in cu-runtime::Session::transition()
```

- Only one session is active at a time; starting a second session while one is
  active is rejected unless the existing one is stopped.
- The **control lock** is held by the session. Taking it over (user) flips the
  session to `user_takeover`; the runtime refuses further actions with
  `USER_TAKEOVER` until the session is released/stopped.
- **Pause** (`PAUSED`) and **stop** (`INVALID_SESSION_STATE`) gate every
  observe/act, not just act.

## Stale-frame policy

1. `observe` stores the frame on disk (`~/.computer-use/frames/s_<id>_<n>.jpg`)
   and returns a monotonically increasing `frame_id` (per session).
2. `act`/`inspect` reference a `frame_id`; the runtime compares the stored
   frame against a fresh quick snapshot of the live desktop.
3. The frame is stale (`STALE_FRAME`) when the pixel diff exceeds the
   threshold (64×64 grayscale thumbnail, 0.12) or the frame id is older than
   `DEFAULT_MAX_FRAME_AGE_SECS` (120s, wall-clock backstop). A frame whose
   content still matches the live screen — e.g. an older id over an unchanged
   desktop — remains actionable.
4. A change of the frontmost application is always stale
   (`DEFAULT_APP_CHANGE_IS_STALE`), even if pixels look similar.

## Stabilizer

`WaitPolicy` on actions:

- `none` — act immediately.
- `fixed` — wait `fixed_wait_ms` (in the runtime, not the client, so traces
  stay honest), then re-verify freshness and act.
- `until_stable` — sample thumbnails every 200ms; act once 3 consecutive
  samples differ by less than 0.02, bounded by 8s.

## Tracing and redaction

Every session writes `~/.computer-use/traces/s_<id>.jsonl`:

```jsonl
{"seq":0,"event":"session.start","session_id":"s_abc","result":{...}}
{"seq":1,"event":"observe","frame_id":"frame_0","result":{...}}
{"seq":2,"event":"action","frame_id":"frame_0","action":{"type":"type","text_redacted":true},"redaction":{"text_redacted":true,"character_count":11}}
```

- `type` actions record **no text** unless `trace_dev_mode` is enabled
  (`COMPUTER_USE_TRACE_DEV_MODE=1 cu daemon start`). The redaction metadata
  (`character_count`) is always recorded.
- Clipboard `paste` actions never record clipboard contents.
- `trace.export` copies the JSONL out; `trace.replay` re-runs the recorded
  actions on the live desktop; `trace.get` returns parsed entries.

## Security properties (enforced server-side)

| Property | Mechanism |
|---|---|
| Only the current user can talk to the daemon | socket mode 0700, owned by the user, stale sockets removed at startup |
| No secret leakage in traces | redaction (above) |
| No acting on stale reality | `STALE_FRAME` (above) |
| No acting off-screen | `OUT_OF_BOUNDS` on every coordinate |
| No acting while paused | `PAUSED` |
| No acting while the user drives | `USER_TAKEOVER` (auto-pause on human input) |
| No acting on a dead session | `INVALID_SESSION_STATE` |
| No acting without the lock | `CONTROL_LOCKED` |
| A wedged bridge can't wedge the daemon | 30s poll-bounded bridge I/O |
| Screenshots never leave the machine unless asked | frames stay under `~/.computer-use/frames/`; `include_image` is per-request |
