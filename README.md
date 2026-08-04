# computer-use

A **model-agnostic, vision-first Computer Use runtime for macOS** (macOS 14+).

External agents (Claude Code, Pi, OpenCode, Codex CLI, or any model) drive the
desktop through a small JSON-RPC surface: capture screenshots, act on them with
clicks/keys/typing, and stay safe behind session locking, stale-frame
protection, and trace redaction. The runtime has **no model dependency** — it
never calls an LLM. The loop is always: *agent observes → agent decides →
runtime executes*.

```
+----------------+   JSON-RPC 2.0    +-------------------+   line-JSON   +---------+
| agent (Pi,     | <===============> | cu-daemon         | <===========> | cubridge|
| OpenCode, ...) |   ~/.computer-use | sessions, locking, |  Unix pipe   | Swift:  |
| via SDK / MCP  |   /runtime.sock   | stale-frame, trace |              | SCK     |
+----------------+                   +-------------------+              | capture |
                                     | cu-runtime · cu-driver-macos      +---------+
```

## What's inside

| Component | Where | Purpose |
|---|---|---|
| `cu` CLI | [crates/cu-cli](crates/cu-cli) | daemon lifecycle, session, observe/act, traces |
| Daemon | [crates/cu-daemon](crates/cu-daemon) | JSON-RPC 2.0 over a Unix socket (current-user only) |
| Runtime | [crates/cu-runtime](crates/cu-runtime) | sessions, control lock, action queue, stabilizer, pause/resume/takeover/stop |
| macOS driver | [crates/cu-driver-macos](crates/cu-driver-macos) | capture, mouse, keyboard, displays, clipboard, permissions |
| Swift bridge | [crates/cu-driver-macos/swift](crates/cu-driver-macos/swift) | ScreenCaptureKit + clipboard + AX (the only Swift in the project) |
| Trace recorder | [crates/cu-trace](crates/cu-trace) | session JSONL traces with redaction |
| TypeScript SDK | [packages/sdk-typescript](packages/sdk-typescript) | `ComputerUseClient` for Node agents |
| MCP Server | [packages/mcp-server](packages/mcp-server) | 7 tools (observe/act/inspect/session/cancel/trace) as image content blocks |
| Pi Extension | [packages/pi-extension](packages/pi-extension) | 4 tools with real image content blocks + 8 slash commands, abort + lifecycle |
| OpenCode adapter | [packages/opencode-adapter](packages/opencode-adapter) | companion CLI (`cu-opencode`) + official MCP config for OpenCode |
| Inspector | [apps/cu-inspector](apps/cu-inspector) | minimal local dashboard (http://127.0.0.1:8420) |

## Quick start

```bash
# 1. build
cargo build --release

# 2. grant permissions once (see docs/permissions.md):
#    System Settings → Privacy & Security → Screen Recording → add cubridge

# 3. start the daemon
cu daemon start

# 4. drive it
cu doctor
cu session start
cu observe --include-image --image-out /tmp/screen.jpg
cu move 500 400
cu click 500 400
cu type "hello"            # text is redacted in traces
cu session stop
```

Type actions are **redacted by default**: traces record `text_redacted: true`
and a character count, never the text itself. To log full text (e.g. a
development environment you trust), run the daemon with dev mode on — see
[Trace redaction](#trace-redaction).

## The four tools (any agent)

| Tool | Purpose |
|---|---|
| `computer_observe` | Capture the screen → frame_id + image + metadata |
| `computer_act` | Execute actions on a frame (click, move, type, key, scroll, drag, wait) |
| `computer_inspect` | Crop a region of a stored frame (vision detail, no DOM/XPath/OCR) |
| `computer_session` | Start / status / pause / resume / takeover / release / stop |

Plus trace inspection (`trace_list`, `trace_get`, `trace_export`,
`trace_replay`) and runtime introspection (`health`, `permissions`, `displays`,
`pointer`, `active-application`).

Everything the runtime enforces — frame staleness, coordinates in bounds,
pause, takeover, session state, the control lock — is enforced **server-side**,
not by the client, so every adapter gets the same guarantees.

## Security model

- **Socket**: Unix domain socket at `~/.computer-use/runtime.sock`, mode `0700`
  — only your user can connect.
- **Sessions**: one active session at a time; every observe/act carries a
  `session_id`. Actions on a stale, paused, taken-over, or stopped session are
  rejected with a specific error code.
- **Stale frames**: acting on anything but the session's current frame is
  rejected (`STALE_FRAME`) under the default `strict` policy; the
  `visual_match` policy (env `COMPUTER_USE_STALE_POLICY`) additionally
  allows an older frame whose content still matches the live screen. Live
  visual comparison + app-change + age backstop always run on top.
- **Bounds**: actions outside the display are rejected (`OUT_OF_BOUNDS`).
- **Redaction**: `type` records `{ text_redacted: true, character_count }` in
  traces; full text only under an explicit opt-in.
- **Takeover**: a human can grab the mouse at any time; the session flips to
  `user_takeover` and the runtime refuses further actions. `resume` cannot
  bypass it — the agent must `release` first (`USER_TAKEOVER_ACTIVE`).
- See [docs/protocol.md](docs/protocol.md) for the full error table and
  [docs/permissions.md](docs/permissions.md) for the permission gotchas
  (including the "rebuild cubridge → re-grant Screen Recording" one).

## Trace redaction

Default: on. `cu daemon start` runs with redaction. To record full typed text
in traces (development only):

```bash
COMPUTER_USE_TRACE_DEV_MODE=1 cu daemon start
```

Each trace entry keeps `redaction: { text_redacted, character_count }` so you
can audit what happened without exposing secrets.

Trace recording policy (`COMPUTER_USE_TRACE_MODE`): `best_effort` (default —
a trace write failure degrades the trace and `computer.act` reports
`trace: {degraded: true, warnings}`), `required` (session start / act fail if
the trace cannot be recorded), or `disabled` (no recorder).

## Layout

```
~/.computer-use/
├── runtime.sock        # JSON-RPC socket (0700)
├── bin/cubridge        # compiled Swift bridge
├── frames/             # captured frames (per session, named s_<id>_<n>.jpg)
├── traces/             # s_<id>.jsonl session traces
└── daemon.log
```

## Tests

```bash
cargo test --workspace                    # 140 tests (Rust: core, driver, runtime, daemon protocol)
cargo test -p cu-daemon --test integration -- --ignored   # live security-matrix test
pnpm install && pnpm -r build && pnpm -r test             # 47 tests: SDK (17), MCP (9), Pi (9), OpenCode (10), inspector (2)
./scripts/smoke.sh                        # automated smoke: gates + Pi/OpenCode wiring snapshots
./scripts/acceptance.sh                   # 18-step manual acceptance (needs a GUI session)
```

## Documentation

- [docs/architecture.md](docs/architecture.md) — components, threads, data flow
- [docs/protocol.md](docs/protocol.md) — JSON-RPC surface, methods, error codes
- [docs/permissions.md](docs/permissions.md) — Screen Recording / Accessibility setup & troubleshooting
- [docs/acceptance-manual.md](docs/acceptance-manual.md) — Pi (15 steps) + OpenCode (11 steps) manual acceptance checklist
- [docs/uninstall.md](docs/uninstall.md) — clean removal
- [packages/sdk-typescript/README.md](packages/sdk-typescript/README.md)
- [packages/mcp-server/README.md](packages/mcp-server/README.md)
- [packages/pi-extension/README.md](packages/pi-extension/README.md)
- [packages/opencode-adapter/README.md](packages/opencode-adapter/README.md)

## License

MIT (see [LICENSE](LICENSE)).
