# computer-use

[![CI](https://github.com/taotao135791-bit/oc-computer-use/actions/workflows/ci.yml/badge.svg)](https://github.com/taotao135791-bit/oc-computer-use/actions/workflows/ci.yml)

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
cu observe --include-image --image-out /tmp/screen.jpg   # first observe auto-creates a session
cu move 500 400
cu click 500 400
cu type "hello"            # text is redacted in traces
cu session stop            # only the client that started the session may stop it
```

**Sessions are created on first use.** The first `observe`/`act` from any
client auto-starts a session when none is active (the CLI resolves the active
session first and only starts when the daemon reports `SESSION_NOT_FOUND`).
The daemon records **who** started it — every client sends its identity
(`client_id` / `client_name` / `client_instance_id`) with `session start`, and
`session status` returns the owner. Ownership matters: a session may be
stopped by the client that created it (a second client trying to use it gets
`CONTROL_LOCKED` under the default policy — see the Pi extension's
`COMPUTER_USE_EXISTING_SESSION_POLICY`).

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
- **Sessions**: one active session at a time (control lock). Auto-creation is
  an *adapter* convenience (SDK/CLI/MCP/Pi resolve `status` first and start
  only on `SESSION_NOT_FOUND`) — the raw `computer.observe` / `computer.act`
  methods never create a session. The creator is recorded as the session's
  **owner** and is the only client that stops it. A session owned by another
  client is refused with `CONTROL_LOCKED`. Every observe/act carries a
  `session_id`. Actions on a stale, paused, taken-over, or stopped session
  are rejected with a specific error code.
- **Capability tokens**: `session start` returns a session's **two tokens
  exactly once** (each 256-bit CSPRNG): an `observation_token` for sensitive
  reads and a `control_token` for mutating operations (which also opens
  reads). **Knowing a session ID grants no observation or control
  permission** — the daemon verifies SHA-256 hashes of presented tokens and
  never repeats them after `start`. `status` never re-issues them, and `stop`
  or a daemon restart invalidates them. The CLI persists session credentials
  to files with mode `0600`; the SDK keeps them in memory only.
- **Existing sessions default to `reject`**: a client that finds a session it
  does not own must not silently attach — `read_only` (observe-only, no
  token held) and `attach_with_token` (caller supplies the token) are
  explicit opt-ins.
- **Daemon admin token**: `runtime.shutdown` requires a per-install admin
  token (256-bit CSPRNG, persisted `0600` at daemon startup) — only the
  daemon manager (CLI / LaunchAgent) holds it; a corrupt store refuses
  startup rather than leaving the daemon unstoppable.

### Capability matrix

| Operation | Session ID alone | Observation token | Control token | Admin token |
|---|---|---|---|---|
| `status` | `OBSERVATION_TOKEN_REQUIRED` | ✅ | ✅ | — |
| `observe` / `inspect` | `OBSERVATION_TOKEN_REQUIRED` | ✅ | ✅ | — |
| trace list / get / export / replay | `OBSERVATION_TOKEN_REQUIRED` | ✅ | ✅ | — |
| `act` / `cancel` / `pause` / `resume` / `takeover` / `release` / `stop` | `CONTROL_TOKEN_REQUIRED` | ❌ | ✅ | — |
| `runtime.shutdown` | `DAEMON_ADMIN_TOKEN_REQUIRED` | ❌ | ❌ | ✅ |

The control token includes observation permission (it verifies for reads
too); the observation token never grants mutation. Token errors are
deliberately non-descriptive (`INVALID_*` never says which token was wrong).

- **Stale frames**: acting on anything but the session's current frame is
  rejected (`STALE_FRAME`) under the default `strict` policy; the
  `visual_match` policy (env `COMPUTER_USE_STALE_POLICY`) additionally
  allows an older frame whose content still matches the live screen. Live
  visual comparison + app-change + age backstop always run on top.
- **Bounds**: actions outside the display are rejected (`OUT_OF_BOUNDS`).
- **Redaction**: `type` records `{ text_redacted: true, character_count }` in
  traces; full text only under an explicit opt-in. Clipboard contents are
  never recorded, and no capability token ever appears in a trace.
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
cargo test --workspace                    # 175 tests (Rust: core, driver, runtime, daemon protocol, ownership matrix)
cargo test -p cu-daemon --test integration -- --ignored   # live security-matrix test
pnpm install && pnpm -r build && pnpm -r test             # 80 tests: SDK (33), Pi (14), OpenCode adapter (23), MCP (10)
./scripts/smoke.sh                        # automated smoke: gates + Pi/OpenCode wiring snapshots
```

Real-environment acceptance (needs a logged-in GUI session, Screen
Recording + Accessibility permissions, daemon running, no active session):

```bash
node scripts/pi-host-acceptance.mjs       # Pi extension, real code, real daemon/screen — 32 checks
node scripts/opencode-mcp-acceptance.mjs  # real computer-use-mcp binary over stdio, real daemon/screen — 17 checks
node scripts/ownership-scenario-a.mjs     # ownership: MCP-owned session vs. the Pi extension — 6 checks
```

See [docs/acceptance-manual.md](docs/acceptance-manual.md) for the full
manual checklists (Pi 20 steps, OpenCode 14 steps, ownership A/B/C) and the
results recorded during the round-2 and round-3 acceptance runs.

## Documentation

- [docs/architecture.md](docs/architecture.md) — components, threads, data flow
- [docs/protocol.md](docs/protocol.md) — JSON-RPC surface, methods, error codes, session behavior (auto-create, ownership, cancel)
- [docs/permissions.md](docs/permissions.md) — Screen Recording / Accessibility setup & troubleshooting
- [docs/acceptance-manual.md](docs/acceptance-manual.md) — Pi (20 steps) + OpenCode (14 steps) manual acceptance checklist, with round-2 and round-3 results
- [docs/uninstall.md](docs/uninstall.md) — clean removal
- [packages/sdk-typescript/README.md](packages/sdk-typescript/README.md)
- [packages/mcp-server/README.md](packages/mcp-server/README.md)
- [packages/pi-extension/README.md](packages/pi-extension/README.md)
- [packages/opencode-adapter/README.md](packages/opencode-adapter/README.md)

## License

MIT (see [LICENSE](LICENSE)).
