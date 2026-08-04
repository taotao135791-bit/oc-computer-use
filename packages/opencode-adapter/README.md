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
| `cu-opencode setup` | adds the computer-use MCP server to `~/.config/opencode/opencode.json` (merges with existing config) |
| `cu-opencode setup --print` | prints the config fragment to stdout |
| `cu-opencode status` | daemon health (version, permissions, active sessions) + current session state |
| `cu-opencode session cleanup` | stops the active session (idempotent) |
| `cu-opencode doctor` | checks `cu`/`computer-use-mcp` binaries, socket, and daemon health; exits nonzero on issues |
| `cu-opencode help` | usage |

Also ships `config/opencode.config.json` — a ready-to-copy config with the
MCP entry.

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
four tools — `computer_session`, `computer_observe`, `computer_act`,
`computer_inspect` — with structured zod schemas (actions are a
discriminated union, `region` is an object; no JSON-string-inside-JSON).

## Smoke test (OpenCode)

```bash
cu daemon start
cu-opencode status            # daemon ready, no session
opencode                      # start an agent session
```

1. Ask: "use computer_session to start a session" → expect the session state
   and a `session_id`.
2. Ask: "use computer_observe to look at the screen" → expect a real
   screenshot image (not a path or a file download).
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

## Library use

All functionality is exported from `dist/index.js` as plain async functions
(`generateOpenCodeConfig`, `writeOpenCodeConfig`, `statusText`,
`cleanupSession`, `doctor`, `doctorText`, `defaultOpenCodeConfigPath`), so
scripts and tests can drive it without spawning the CLI.

## Tests

```bash
pnpm test   # 10 tests (fake daemon)
```
