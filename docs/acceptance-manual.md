# Manual acceptance checklist

Automated coverage lives in the test suites (`cargo test --workspace`,
`pnpm -r test`, `./scripts/smoke.sh`). The checklists below exercise the two
agent integrations end-to-end against a real display — these are the steps
that need a logged-in GUI session and Screen Recording + Accessibility
permissions (see [permissions.md](permissions.md)).

**Round 2 (2026-08-04)** was run against the real daemon and screen on
macOS 26.0 (arm64): the Pi steps via a minimal Pi host shim loading the real
extension code, the OpenCode steps via the real `computer-use-mcp` binary
speaking MCP over stdio. Results are recorded per step below.

**Setup once**

```bash
cargo build --release
cu daemon start
pnpm install && pnpm -r build
pnpm --filter @computer-use/opencode-adapter build
cu-opencode setup          # from the adapter package
```

The same flows are automated by:

```bash
node scripts/pi-host-acceptance.mjs       # Pi steps 1-20 against real daemon/screen
node scripts/opencode-mcp-acceptance.mjs  # OpenCode steps 1-13 against real daemon/screen
node scripts/ownership-scenario-a.mjs     # ownership scenario A (MCP-owned session)
```

## Pi (pi-coding-agent) — 20 steps

Prereqs: `packages/pi-extension` built, loaded in Pi
(`~/.pi/agent/extensions` or project `.pi/extensions`), `/reload` applied.

> Steps marked `(host shim)` were verified by loading the **real extension
> code** in a minimal host that implements Pi's Extension API
> (`registerTool`/`registerCommand`/`session_shutdown`/`ctx.ui.notify`) —
> the daemon and screen were real. Steps that additionally need the Pi app
> itself (rendering the image in the chat, Pi's real AbortSignal wiring) are
> marked **NOT VERIFIED** — the Pi host app was not installed on the
> acceptance machine.

| # | Step | Expected result | Round-2 result |
|---|---|---|---|
| 1 | daemon up | `cu daemon status` → running | ✅ PASS |
| 2 | no active session | `cu session status --json` → SESSION_NOT_FOUND | ✅ PASS |
| 3 | host loads the extension | loads without error | ✅ PASS (host shim) |
| 4 | extension registers 4 tools + 8 commands | `computer_session/observe/act/inspect` + `/computer-*` commands | ✅ PASS (host shim) |
| 5 | `/computer-status` | notification with real daemon version, permissions, session state | ✅ PASS (host shim) |
| 6 | first `computer_observe` (model tool) | succeeds — **auto-creates** the session | ✅ PASS (host shim) |
| 7 | observe returns a **real screenshot image** | image content block, JPEG magic bytes, `frame_id` + size in text | ✅ PASS (host shim); in-Pi chat rendering **NOT VERIFIED** (Pi app not installed) |
| 8 | session auto-created by the extension | `cu session status` → owner `pi-extension`/`Pi`/`pi-<pid>-…`; exactly **one** session start | ✅ PASS (host shim) |
| 9 | `computer_act` with a move+wait | per-action `success (Nms)` + post-batch screenshot image | ✅ PASS (host shim); in-Pi rendering **NOT VERIFIED** |
| 10 | `computer_inspect` on a region | cropped image content block + `global_origin`/`normalized_1000_origin` mapping | ✅ PASS (host shim) |
| 11 | `computer_inspect` image visible to the model | image block present in the tool result | ✅ PASS (host shim); in-Pi rendering **NOT VERIFIED** |
| 12 | `/computer-takeover` | session state → `user_takeover: true` | ✅ PASS (host shim) |
| 13 | act while taken over | rejected with `USER_TAKEOVER` | ✅ PASS (host shim) |
| 14 | `/computer-resume` while taken over | **cannot bypass** — still `user_takeover`, error `USER_TAKEOVER_ACTIVE` | ✅ PASS (host shim) |
| 15 | `/computer-release` | state back to `active` | ✅ PASS (host shim) |
| 16 | act after release | succeeds | ✅ PASS (host shim) |
| 17 | `/computer-observe` (screenshot save) | file in **system temp dir** `oc-computer-use-<session>-<frame>.jpg`, real JPEG, mode `0600`; nothing written to cwd/repo | ✅ PASS (host shim) |
| 18 | quit / reload Pi with an active session | `session_shutdown` fires; the extension stops **its own** session | ✅ PASS (host shim; real Pi event wiring NOT VERIFIED) |
| 19 | screenshot cleaned up on shutdown | temp screenshot file gone after stop | ✅ PASS (host shim) |
| 20 | foreign session (created by another client) | default `reject` → tools fail `CONTROL_LOCKED`, session untouched; `COMPUTER_USE_EXISTING_SESSION_POLICY=attach` → observe/act work, shutdown does **not** stop the foreign session | ✅ PASS (host shim; scenarios A/B) |
| — | `type` redaction in traces | trace entry shows `text_redacted: true` + character count, never the text | ✅ PASS (CLI verification) |

## OpenCode — 14 steps

Prereqs: `cu-opencode setup` ran, `computer-use-mcp` on PATH, OpenCode
restarted. Steps 1-13 are model-free (verified by driving the real MCP
binary over stdio); step 14 is the config write. Steps that need a model
choosing to call the tools are **NOT VERIFIED** — the zhipuai GLM Coding
Plan on the acceptance machine expired during the round-2 run.

| # | Step | Expected result | Round-2 result |
|---|---|---|---|
| 1 | daemon up, no active session | `cu daemon status` ok; status → SESSION_NOT_FOUND | ✅ PASS |
| 2 | `computer-use-mcp` starts and initializes | MCP `initialize` → serverInfo `computer-use` | ✅ PASS |
| 3 | `tools/list` | 7 tools: `computer_session/observe/act/inspect/cancel/trace_list/trace_get` | ✅ PASS |
| 4 | first `computer_observe` | succeeds — **auto-creates** the session | ✅ PASS |
| 5 | model receives a real screenshot | image content block, JPEG magic bytes | ✅ PASS |
| 6 | session owned by the MCP server | status → owner `mcp-server`/“Computer Use MCP” | ✅ PASS |
| 7 | second observe | reuses the session — no second start | ✅ PASS |
| 8 | `computer_act` (move + wait) | per-action `success`, `next_frame_id` | ✅ PASS |
| 9 | switch windows, then act on the **old** frame | rejected `STALE_FRAME` | ✅ PASS |
| 10 | re-observe, act again | succeeds | ✅ PASS |
| 11 | cancel a 30 s wait | `computer_cancel` ack; the wait returns `action[0]: cancelled` in < 1 s (787 ms) | ✅ PASS |
| 12 | `computer_session stop` | state `stopped` | ✅ PASS |
| 13 | status after stop | no active session | ✅ PASS |
| 14 | JSONC config write (`cu-opencode setup`) | merges into a **JSONC** config (comments/trailing commas preserved, other keys untouched, only `mcp.computer-use` added), backup `opencode.json.backup-<ts>` only on change, second run idempotent | ✅ PASS (verified on the real `~/.config/opencode/opencode.json`) |
| — | model-driven calls (model selects tools, image rendered in OpenCode UI) | — | **NOT VERIFIED** — GLM Coding Plan expired; needs a working provider |

## Ownership scenarios (A / B / C)

| Scenario | Steps | Round-2 result |
|---|---|---|
| A — OpenCode/MCP owns the session | MCP creates the session → Pi extension (real code) uses it → CONTROL_LOCKED (default reject) → Pi shutdown → **session still active** | ✅ PASS (scripts/ownership-scenario-a.mjs, 6/6) |
| B — Pi owns the session | Pi creates via first observe → Pi `session_shutdown` → session stopped, screenshot cleaned | ✅ PASS (Pi steps 8/18/19) |
| C — CLI owns the session, Pi must not take over | CLI starts → Pi (reject) → CONTROL_LOCKED; with `attach` → usable, shutdown does not stop it | ✅ PASS (Pi step 20) |

**Round 3 (2026-08-04)** added the server-side session-ownership round
(control token, ExistingSessionPolicy, precise cancel). Results recorded per
step below; all automated scripts pass against the round-3 daemon.

| # | Step | Expected result | Round-3 result |
|---|---|---|---|
| 1 | `cu session start --json` | `control_token` present but `<redacted>` — the token is issued exactly once and never printed | ✅ PASS |
| 2 | `cu session status --json` | **no** `control_token` field (read-only never repeats it); `owner_client_id`/`owner_client_name` present | ✅ PASS |
| 3 | second `cu session start` | `CONTROL_LOCKED` naming the holder; the lock is held by the first session | ✅ PASS |
| 4 | stop with a wrong token (raw wire request) | `INVALID_CONTROL_TOKEN`; the session stays active (no side effects) | ✅ PASS |
| 5 | CLI credential file | `~/.local/state/oc-computer-use/credentials/<sid>.json`, mode `0600` | ✅ PASS |
| 6 | `cu session stop` (owner) | succeeds; credential file deleted with the session | ✅ PASS |
| 7 | daemon restart | in-memory session ends, token invalid; stale operations fail cleanly (SESSION_NOT_FOUND) | ✅ PASS |
| 8 | Pi extension against the round-3 daemon | full matrix 31/31 PASS incl. scenario C with the new wire shape (`CONTROL_LOCKED` message + owner in `data`) | ✅ PASS (host shim) |
| 9 | OpenCode/MCP against the round-3 daemon | 17/17 PASS incl. cancel of a 30 s wait in 761 ms; session owned by `mcp-server` | ✅ PASS |
| 10 | ownership scenario A | 6/6 PASS — Pi shutdown does not stop the MCP-owned session | ✅ PASS |

**Round 4 (2026-08-05)** — protocol v3 (capability tokens, daemon admin
token). The Pi extension was re-verified against the v3 daemon:
`scripts/pi-host-acceptance.mjs` → **32/32 PASS** (host shim; real extension
code, real daemon, real screen). The v3 semantics are what changed, so the
round-4 differences are recorded explicitly:

- `cu session status` on a session the CLI does **not** own is refused
  (`OBSERVATION_TOKEN_REQUIRED`) — the acceptance script now reads Pi-session
  state through the extension's own `computer_session` tool (owner check:
  `pi-extension`/`Pi`/`pi-<pid>-…`). The CLI still reads sessions it owns.
- `COMPUTER_USE_EXISTING_SESSION_POLICY=attach` prints the deprecation
  warning and maps to `read_only` (Pi step 20's round-2 "attach → observe
  works" no longer exists: a session id alone grants no observation
  permission).
- a tokenless `read_only` attach is refused with `INVALID_PARAMS` + the
  `attachReadOnly` hint; the foreign session stays active, and the read_only
  client's `session_shutdown` never stops it.
- the acceptance script now exits explicitly — an all-PASS run previously
  left the extension's SDK sockets open and hung piped runs (tail/CI) forever.
- In-Pi-app steps (chat rendering, Pi's real AbortSignal wiring,
  `session_shutdown` fired by the Pi app itself) remain **NOT VERIFIED** —
  the Pi host app is still not installed on the acceptance machine; the
  extension is verified in a host shim against the real daemon/screen.

The OpenCode/MCP side was also re-verified against the v3 daemon:
`scripts/opencode-mcp-acceptance.mjs` → **17/17 PASS** (real
`computer-use-mcp` binary over stdio, real daemon, real screen), and the real
OpenCode host loaded the server: `opencode mcp list` reports **`computer-use`
connected** (entry present in `~/.config/opencode/opencode.json`, command
`computer-use-mcp`). Round-4 differences:

- like the Pi script, session state is read through the MCP's own
  `computer_session` tool (the CLI can no longer read a session it does not
  own — `OBSERVATION_TOKEN_REQUIRED`); the initial no-session check now
  distinguishes a clean `SESSION_NOT_FOUND` from a foreign live session.
- the script now SIGTERMs the MCP server and waits for it to run its
  `stopOwnedSessionOnExit` cleanup — a failed run previously left an orphaned
  MCP process whose session locked the daemon for the next run.
- model-driven steps (a model choosing to call the tools, image rendered in
  the OpenCode UI) remain **NOT VERIFIED** — the GLM Coding Plan on the
  acceptance machine is still expired (see the round-2 failure record).

**Round 5 (2026-08-05)** — release hardening: sensitive-read capability
tokens, credential-file write safety, graceful shutdown, removed session
policies. Both integrations re-verified against the round-5 daemon:

- `scripts/pi-host-acceptance.mjs` → **32/32 PASS** (host shim; real
  extension code, real daemon, real screen). Round-5 differences:
  - the removed `read_only`/`attach` policies are gone from the wire: both
    env values print the deprecation warning and behave exactly like the
    only policy, `reject` — the scenario now asserts `CONTROL_LOCKED` +
    warning for both, and that a rejected client's `session_shutdown` never
    stops the foreign session.
  - sensitive reads in the scenario (session state, screenshot) all flow
    through the session's capability tokens — the acceptance script reads
    Pi-session state via the extension's own `computer_session` tool.
  - screenshots: saved under the system temp dir, mode `0600`, real JPEG
    magic, cleaned up on shutdown — re-asserted in the round-5 run.
- `scripts/opencode-mcp-acceptance.mjs` → **17/17 PASS** (real
  `computer-use-mcp` binary over stdio, real daemon, real screen),
  including cancel of a 30 s wait in 750 ms and STALE_FRAME rejection
  after a window switch.
- graceful shutdown verified live: `cu daemon stop` drains the socket,
  cancels in-flight actions (`cancelled`, `executed: false` — never a
  JSON-RPC error), removes the socket + admin token, and the daemon
  restarts cleanly on the same paths.

Round-5 environment fix worth recording: the OpenCode acceptance script
failed with `spawn computer-use-mcp EACCES` because the **globally
installed** copy of the MCP server (`~/.npm-global/lib/node_modules/
@computer-use/mcp-server/dist/bin.js`) had lost its executable bit —
the workspace build artifact keeps `+x` (the add-shebang build step), but
the npm-installed copy did not. Fix: `chmod +x` on the installed copy.
If that ever recurs, run `computer-use-mcp` directly and check the file
mode before blaming the script.

## Failure record (round 2)

| Date | Step # | Observed | Root cause / fix |
|---|---|---|---|
| 2026-08-04 | Pi setup | `osascript` controlling TextEdit hung | macOS automation prompt; use `open -a TextEdit` |
| 2026-08-04 | OpenCode | "Unexpected server error" on first run | `model: glm-4.6` → OpenCode 1.18 renamed vision model to `glm-4.6v`; fixed via `--model` (config untouched by design) |
| 2026-08-04 | OpenCode | model provider calls fail | zhipuai GLM Coding Plan expired (account issue, not repairable here) → model-driven steps NOT VERIFIED |
| 2026-08-04 | Pi | in-Pi-app steps (3/6/8/9/11 model-call parts, image rendering) | Pi host app not installed; extension verified in a host shim against the real daemon/screen |
