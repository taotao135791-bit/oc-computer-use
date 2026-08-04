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

## Failure record (round 2)

| Date | Step # | Observed | Root cause / fix |
|---|---|---|---|
| 2026-08-04 | Pi setup | `osascript` controlling TextEdit hung | macOS automation prompt; use `open -a TextEdit` |
| 2026-08-04 | OpenCode | "Unexpected server error" on first run | `model: glm-4.6` → OpenCode 1.18 renamed vision model to `glm-4.6v`; fixed via `--model` (config untouched by design) |
| 2026-08-04 | OpenCode | model provider calls fail | zhipuai GLM Coding Plan expired (account issue, not repairable here) → model-driven steps NOT VERIFIED |
| 2026-08-04 | Pi | in-Pi-app steps (3/6/8/9/11 model-call parts, image rendering) | Pi host app not installed; extension verified in a host shim against the real daemon/screen |
