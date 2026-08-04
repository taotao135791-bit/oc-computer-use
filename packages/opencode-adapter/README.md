# @computer-use/opencode-adapter

Companion tooling for using the [computer-use runtime](../../README.md) from
[OpenCode](https://opencode.ai).

**How OpenCode connects:** through the runtime's **MCP server**
(`@computer-use/mcp-server`, binary `computer-use-mcp`), using OpenCode's
official MCP config format. This package does **not** re-implement the tools
as an OpenCode plugin — MCP is the single supported wiring.

## What it provides

`cu-opencode` — a CLI that keeps that wiring healthy:

| Command | What it does |
|---|---|
| `cu-opencode setup` | adds the computer-use MCP server to `~/.config/opencode/opencode.json` (JSONC-aware merge, see below) |
| `cu-opencode setup --print` | prints the config fragment to stdout |
| `cu-opencode status` | daemon health (version, permissions, active sessions) + current session state |
| `cu-opencode session cleanup` | stops the sessions **this machine owns** — the ones started by this machine's clients (idempotent) |
| `cu-opencode doctor` | checks `cu`/`computer-use-mcp` binaries, socket, and daemon health; exits nonzero on issues |
| `cu-opencode help` | usage |

Also ships `config/opencode.config.json` — a ready-to-copy config with the
MCP entry.

## JSONC-aware config writes

OpenCode's config is JSONC — comments and trailing commas are legal and
commonly present. `cu-opencode setup` therefore:

- parses and edits with **`jsonc-parser`'s edit API** (never
  parse→modify→stringify, which would drop comments);
- **only touches the `mcp.computer-use` entry** — every other key
  (`$schema`, `model`, `provider`, `permission`, plugins, other MCP
  servers…) is left byte-for-byte untouched, comments and trailing commas
  included;
- writes a backup `opencode.json.backup-<timestamp>` **only when the file
  actually changes** (an up-to-date config produces no backup and a
  "config already up to date" message);
- is idempotent — repeated runs converge.

Covered by 23 tests, including plain JSON, line/block comments, trailing
commas, pre-existing MCP entries, corrupt configs (never overwritten),
idempotency, and session-cleanup ownership (stops only sessions this machine
owns, via its stored credentials).

## Install

From the repo (workspace):

```bash
pnpm --filter @computer-use/opencode-adapter build
pnpm --filter @computer-use/opencode-adapter link  # puts cu-opencode on PATH
```

or as a published package:

```bash
pnpm add -g @computer-use/opencode-adapter @computer-use/mcp-server
```

## Wire OpenCode to the runtime

The MCP server must be installed so `computer-use-mcp` is on PATH (or edit
the `command` to its absolute path).

```bash
# 1. start the daemon
cu daemon start

# 2. generate the OpenCode config
cu-opencode setup            # writes ~/.config/opencode/opencode.json
cu-opencode doctor           # verify everything is reachable
```

Equivalent hand-written config (`config/opencode.config.json` in this repo):

```json
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "computer-use": {
      "type": "local",
      "command": ["computer-use-mcp"],
      "enabled": true
    }
  }
}
```

Restart OpenCode after changing the config. The MCP server registers the
tools — `computer_session`, `computer_observe`, `computer_act`,
`computer_inspect`, `computer_cancel`, `trace_list`, `trace_get` — with
structured zod schemas (actions are a discriminated union, `region` is an
object; no JSON-string-inside-JSON).

**Session ownership.** The session started by OpenCode's first tool call is
owned by the MCP server (`mcp-server` / "Computer Use MCP"); OpenCode is the
only client that stops it. A Pi extension or other client attempting to use
that session gets `CONTROL_LOCKED` by default and will not stop it — see
[packages/pi-extension](../pi-extension/README.md). **Knowing a session ID
does not grant control**: mutating calls require the session's control
token, which the daemon issues exactly once at `start` — the MCP server holds
it in memory and injects it into its own calls; after a daemon restart the
token is invalid and stale operations fail cleanly (`SESSION_NOT_FOUND` /
`INVALID_CONTROL_TOKEN`) instead of pretending to work.

## Smoke test (OpenCode)

```bash
cu daemon start
cu-opencode status            # daemon ready, no session
opencode                      # start an agent session
```

The steps below need a working model provider in OpenCode (the MCP wiring
itself does not — see the acceptance script below for the model-free path).

1. Ask: "use computer_session to start a session" → expect the session state
   and a `session_id`.
2. Ask: "use computer_observe to look at the screen" → expect a real
   screenshot image (not a path or a file download). The first observe
   auto-creates the session.
3. Ask: "use computer_act to move the mouse to (500, 500) normalized" →
   expect success with a post-batch screenshot.
4. Ask: "use computer_inspect on region (0,0,200,200)" → expect a cropped
   image with a coordinate mapping.
5. Trigger takeover outside the agent (`cu session takeover`) and ask it to
   act → expect `USER_TAKEOVER_ACTIVE`; ask it to release then act again →
   succeeds.
6. `cu-opencode session cleanup` outside → next tool call from OpenCode
   fails with `SESSION_NOT_FOUND` (clean, not stale).
7. Quit OpenCode → `cu-opencode status` shows no session.

**Model-free acceptance:** `node scripts/opencode-mcp-acceptance.mjs` (repo
root) drives the same `computer-use-mcp` binary OpenCode launches, speaking
MCP over stdio against the real daemon and screen — 17 checks, no model
needed. Steps above that depend on a model are marked NOT VERIFIED when the
provider plan is unavailable (see [docs/acceptance-manual.md](../../docs/acceptance-manual.md)
for the round-2 record).

## Library use

All functionality is exported from `dist/index.js` as plain async functions
(`generateOpenCodeConfig`, `writeOpenCodeConfig`, `statusText`,
`cleanupSession`, `doctor`, `doctorText`, `defaultOpenCodeConfigPath`), so
scripts and tests can drive it without spawning the CLI.

## Tests

```bash
pnpm test   # 23 tests: JSONC merge scenarios (comments, trailing commas,
            # pre-existing entries, corrupt config, idempotency) + fake-daemon wiring
            # + cleanup ownership (stops only sessions this machine owns)
```
