# Manual acceptance checklist

Automated coverage lives in the test suites (`cargo test --workspace`,
`pnpm -r test`, `scripts/smoke.sh`). The checklists below exercise the two
agent integrations end-to-end against a real display — these are the steps
that need a logged-in GUI session and Screen Recording + Accessibility
permissions (see [permissions.md](permissions.md)).

**Setup once**

```bash
cargo build --release
cu daemon start
pnpm --filter @computer-use/pi-extension build
pnpm --filter @computer-use/mcp-server build
cu-opencode setup      # from packages/opencode-adapter
```

For each step: run it, verify the expected result, tick the box, and note
anything that differs.

## Pi (pi-coding-agent) — 15 steps

Prereqs: `packages/pi-extension` built, loaded in Pi
(`~/.pi/agent/extensions` or project `.pi/extensions`), `/reload` applied.

| # | Step | Expected result | Done |
|---|---|---|---|
| 1 | `/computer-status` in Pi | notification with real daemon version, permissions, and session state (not a mock) | ☐ |
| 2 | `/computer-start` | session starts; status shows `state=active` | ☐ |
| 3 | Ask the agent for `computer_observe` | a **real screenshot image** renders in the conversation (not a path / download link) + text metadata with `frame_id` | ☐ |
| 4 | Ask for `computer_act` with a click | per-action result `success (Nms)` + a post-batch screenshot image | ☐ |
| 5 | Ask for `computer_act` with `type` "hello world" | success; trace entry shows `text_redacted: true`, never the text | ☐ |
| 6 | Ask for `computer_inspect` on a region | cropped image + `global_origin`/`normalized_1000_origin` in text | ☐ |
| 7 | Without a new observe, act on the **previous** frame_id | `STALE_FRAME` error (strict policy is the default) | ☐ |
| 8 | `/computer-pause`, then act | `PAUSED` error | ☐ |
| 9 | `/computer-resume`, then act | succeeds again | ☐ |
| 10 | `/computer-takeover` (simulating a human grabbing control), then `resume` | `USER_TAKEOVER_ACTIVE` — resume cannot bypass takeover | ☐ |
| 11 | `/computer-release`, then act | succeeds again | ☐ |
| 12 | Act on a coordinate far outside the display | `OUT_OF_BOUNDS` | ☐ |
| 13 | `/computer-observe` | PNG file written into the working directory, viewable | ☐ |
| 14 | Quit / reload Pi while a session is active | `session_shutdown` fires: `/computer-status` shows no active session after reload | ☐ |
| 15 | `/computer-stop` | session stops; subsequent tools return `SESSION_NOT_FOUND` | ☐ |

## OpenCode — 11 steps

Prereqs: `cu-opencode setup` ran (or the `mcp` block added manually), MCP
server package installed so `computer-use-mcp` is on PATH, OpenCode
restarted.

| # | Step | Expected result | Done |
|---|---|---|---|
| 1 | `cu-opencode doctor` | no issues found (binaries, socket, daemon health) | ☐ |
| 2 | OpenCode session: ask for `computer_session` start | real session state + `session_id` returned | ☐ |
| 3 | Ask for `computer_observe` | a real screenshot image content block | ☐ |
| 4 | Ask for `computer_act` (move/click) | success with per-action results | ☐ |
| 5 | Ask for a **structured** action batch (click + wait) | batch executes in order; results per index | ☐ |
| 6 | Ask for `computer_inspect` on region `{x:0,y:0,width:200,height:200}` | cropped image + mapping | ☐ |
| 7 | `cu session takeover` from the terminal, then ask to act | `USER_TAKEOVER_ACTIVE` surfaced to the model | ☐ |
| 8 | `cu session release`, ask to act again | succeeds | ☐ |
| 9 | `cu-opencode session cleanup`, then ask for status | `SESSION_NOT_FOUND` (clean, not a stale session) | ☐ |
| 10 | Restart OpenCode; verify MCP server registered | 4 tools visible (`computer_session/observe/act/inspect`) with structured JSON schemas | ☐ |
| 11 | Quit OpenCode with an active session | `cu-opencode status` shows no active session | ☐ |

## Failure record

| Date | Step # | Observed | Root cause / fix |
|---|---|---|---|
| | | | |
