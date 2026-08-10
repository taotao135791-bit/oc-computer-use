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

**Round 6 (2026-08-05)** — session-scoped traces, MCP npm-executable
package, strict smoke. The Pi/OpenCode acceptance scripts could **not** be
re-run on this host: `cu observe` fails with `CAPTURE_FAILED — bridge
request timed out after 30s (is a macOS permission dialog pending?)` even
though TCC reports `screen_recording: true` — the session has no
interactive WindowServer for ScreenCaptureKit to stream from. **Real-host
Pi/OpenCode acceptance: NOT VERIFIED this round** (environment, not a code
failure). What *was* verified live against the real daemon this round:

- `cu trace list` (daemon-manager `trace.admin_list`): lists both sessions'
  traces; wrong admin token → `INVALID_DAEMON_ADMIN_TOKEN` (-32027);
  tokenless → `DAEMON_ADMIN_TOKEN_REQUIRED` (-32026, covered by the
  integration suite).
- `cu trace get <session>`: observation-token path works against the live
  daemon.
- **Trace access survives a daemon restart**: start a session, stop the
  daemon, start it again — `cu trace get` on the old session still succeeds
  via the persisted access manifest (token hashes only), proving the
  restart-persistence property end-to-end.
- `cu daemon stop` with a stale admin credential is refused (the CLI proves
  `daemon_instance_id` before shutting anything down).

## Round 7 (2026-08-10) — Pointer Isolation closing phase, real-hardware acceptance

Real macOS GUI session (WindowServer live; Screen Recording + Accessibility
granted — `cu doctor` all checks pass), display 1512×982 @ scale 1.0, release
`cu` + the P0-1-fixed `cubridge`, daemon running with the Event Tap genuinely
`active` (log: `state="starting"` → `event tap active on dedicated thread`).

> **Environment caveat recorded honestly:** the machine was in **continuous
> active human use** for the entire run (real mouse input every <1 s; the
> frontmost app kept changing under us, e.g. Chrome → 飞书, and Spaces kept
> switching). Human Always Wins therefore fired constantly and *any* outcome
> that needs the target app to stay frontmost for an interval was blocked by
> the environment, not by the code. Each item below states exactly what was
> and was not verified, with the exact commands.

### Test A — window screenshot isolation: PASS

Window-scoped observe returns exactly the target window's rect and only its
content.

| Check | Procedure | Result |
|---|---|---|
| Basic isolation | `cu session start --bundle-id com.apple.TextEdit --pointer-policy physical_allowed` → `cu observe` on a 656×422 window at (100,100) | capture **656×422** (== window bounds, px==pt at scale 1.0), `active_window: iso_test_doc.txt`; content matches the corresponding crop of an independent `screencapture` reference at **99.5%** (0.5% = JPEG text-edge noise) → no cross-app content |
| Window > max_width | resize window to 1512×300 (> default `max_width` 1440) → `cu observe` | capture **1440×285** — exact `max_width` downscale; height floor-rounded, within the P0-2 ±1 px tolerance |
| Moved window | move window to (500,200), same size → `cu observe` | still **656×422**; capture matches the **new** location's full-screen crop at **99.3%** vs **86.6%** at the old location → crop follows P0-3 refreshed bounds, never stale |
| Fail-closed | close the target window / window identity changes (reopened with a new CGWindowNumber) → `cu observe` / `cu double-click` | refused with **`TARGET_UNAVAILABLE`** — never a stale-bounds capture (observed live, repeatedly) |

### Test B — human conflict (≥20 trials): NOT VERIFIED

The ≥20-trial protocol needs a **cooperating human operator** producing
controlled conflict events; none was available, so the trials were not run.

Incidental real-hardware confirmations of the mechanism were observed:
a genuine human event flipped the session to `user_takeover` and `cu observe`
was refused with `USER_TAKEOVER`; after `release`, the session re-flipped to
`user_takeover` within ~300 ms of the next human event; an in-flight
`cu double-click` was cancelled at event time by the human-interrupt hook
(action `status: "cancelled"`).

Manual acceptance step: with a human physically at the machine and the session
targeted at a known app, repeat 20× — the operator moves the real mouse / types
while the agent acts; assert each time the session flips to `user_takeover`,
further acts are refused with `USER_TAKEOVER`, and no synthetic click happens
after the last human event. Record the P0-4 KPIs
(`event_detection_latency_ms`, `human_to_takeover_ms`, `human_to_input_stop_ms`)
from the trace.

### Test C — Browser board ≥50 clicks: NOT VERIFIED

Requires the board page open and frontmost for ≥50 uninterrupted isolated
clicks; the operator continuously switched the frontmost app / Space and the
session kept re-flipping to `user_takeover`, so ≥50 consecutive accepted clicks
were not possible in this session.

Commands for manual acceptance:

```bash
node benchmarks/target-boards/browser/server.mjs 8765 &          # serves index.html + /api/record|stats|reset
open -a "Google Chrome" "http://127.0.0.1:8765"                  # board on the visible space
cu session start --bundle-id com.google.Chrome --pointer-policy isolated_only
# then a script drives ≥50 cu click <x> <y> at the board's targets (from /api/record),
# and /api/stats reports hit rate + p50 center_error_px
```

### Test D — Native board ≥30 clicks: NOT VERIFIABLE as specified

No `benchmarks/target-boards/native/` board exists in the repo (only
`browser/`). There is nothing to run the ≥30 native clicks against. Would-be
command: same as C with a native board dir serving targets and recording hits.

### DoubleClick real test: PARTIAL — success-outcome NOT VERIFIED

**Verified on hardware:** `cu double-click` executes through the isolated
Direct-CG path. 8 successful double-click actions were traced on the live
machine, every one `backend: "direct_cg_event"`, `isolated: true`,
`physical_cursor_delta_px: 0.0`, `physical_cursor_moved: false`. The driver's
`double_click_direct` posts down/up with `click_state 1` then down/up with
`click_state 2`, so the OS receives a **real double-click** (never two single
clicks) and the real system cursor is never moved.

**NOT VERIFIED:** the word-selection *outcome* (double-click a word in TextEdit
→ the word becomes selected). The operator's continuous activity kept the
frontmost app away from TextEdit (clicks landed on the frontmost app — Chrome /
飞书), and when TextEdit was frontmost the window kept leaving the visible
Space. The physical-fallback double-click path
(`AX_UNSUPPORTED_FOR_DOUBLE_CLICK` → `physical_double_click_at` under
`physical_allowed`) was not exercised on hardware either, because the isolated
path succeeded; it is covered by the cu-runtime unit tests (102 tests).

Manual acceptance step: quiet the machine, then:

```bash
open -a TextEdit /tmp/wordsel.txt
osascript -e 'tell application "TextEdit" to activate'
# find a word's center via AX (kAXBoundsForRangeParameterizedAttribute), e.g. (533, 238)
cu session start --bundle-id com.apple.TextEdit --pointer-policy physical_allowed
cu double-click 533 238
# assert AXSelectedText == the word under the point
```

### Pointer Isolation (Validation Focus #1): PASS

On the live machine every traced action (`move`, `click`, `double-click`)
recorded `isolated: true`, `physical_cursor_delta_px: 0.0`,
`physical_cursor_moved: false`. A `cu move 600 300` left the real system cursor
bit-identical before and after (880,306 → 880,306). The ghost cursor is never
the real cursor; the physical fallback (which does warp) is the only path that
touches the real cursor and it is gated by `physical_allowed` + the human
interrupt checks.

### P1 Event Tap state: PASS

On real hardware the daemon log shows `state="starting"` then
`event tap active on dedicated thread`; `human_input_monitor_state()` reports
`active`. The Starting ≠ Active distinction is real and observable.

### P0-4 interrupt telemetry: PARTIAL

The KPI suffix is emitted on the human-grab-during-fallback failure path
(unit-tested). On this run the isolated path never fell back, so the suffix
path was not hit on hardware; no KPI numbers are claimed from this session.

## Failure record (round 2)

| Date | Step # | Observed | Root cause / fix |
|---|---|---|---|
| 2026-08-04 | Pi setup | `osascript` controlling TextEdit hung | macOS automation prompt; use `open -a TextEdit` |
| 2026-08-04 | OpenCode | "Unexpected server error" on first run | `model: glm-4.6` → OpenCode 1.18 renamed vision model to `glm-4.6v`; fixed via `--model` (config untouched by design) |
| 2026-08-04 | OpenCode | model provider calls fail | zhipuai GLM Coding Plan expired (account issue, not repairable here) → model-driven steps NOT VERIFIED |
| 2026-08-04 | Pi | in-Pi-app steps (3/6/8/9/11 model-call parts, image rendering) | Pi host app not installed; extension verified in a host shim against the real daemon/screen |
