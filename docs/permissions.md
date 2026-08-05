# Permissions & troubleshooting

The runtime needs two macOS permissions. Both are **per-executable**: macOS
keys them to the app/binary that makes the call, not to your account.

## 1. Screen Recording (required for observe)

Required by `cubridge` (ScreenCaptureKit). Without it, captures either fail
fast with a clear error (since v0.1.0 the bridge preflights and reports
"Screen Recording permission is not granted") or, on older builds, hang.

**Grant:** System Settings → Privacy & Security → Screen Recording → add
`~/.computer-use/bin/cubridge` (or `~/Library/Application Support/...` if you
moved it) → toggle it on.

> **Rebuilding `cubridge` resets this permission.** The binary is ad-hoc
> signed; TCC records the signature hash. Any rebuild (a new `swiftc` run, a
> software update that ships a new binary) creates a new hash and the grant
> silently stops applying — the symptom is captures hanging or failing with
> the permission error even though "it worked yesterday".
>
> Fix: re-grant in System Settings (remove + re-add, or just toggle off/on),
> or reset the entry and re-approve the next prompt:
>
> ```bash
> tccutil reset ScreenCapture "$HOME/.computer-use/bin/cubridge"
> ```

## 2. Accessibility (required for reliable keyboard/type, optional otherwise)

Used for typed input reliability and reading the focused window title
(best-effort). Without it, `type`/`key` events may be ignored by many apps.

**Grant:** System Settings → Privacy & Security → Accessibility → add the
daemon's process. Since the daemon is spawned by `cu`, add **both**
`cu`-launched binaries you use (`~/.../cu`, or whatever launched the daemon)
and `~/.computer-use/bin/cubridge`.

> Same rebuild caveat applies.

## Diagnose

```bash
cu doctor          # end-to-end check: socket, permissions, displays
cu permissions     # reports screen_recording / accessibility + guidance
```

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `cu observe` fails with "Screen Recording permission is not granted" | re-grant `cubridge` (above) |
| observe/act hangs forever | very old binary; upgrade, then re-grant |
| the daemon stops responding to *everything* | a wedged bridge used to block the executor; fixed in v0.1.0 — the bridge I/O is bounded (30s) and produces `DRIVER_ERROR` instead |
| typing goes nowhere | Accessibility missing for the process that posts events |
| `daemon start` says unhealthy | check `~/.computer-use/daemon.log`; usually a permission issue surfaced on first observe |
| screen captures are black/blank | Screen Recording granted to a different app; re-check the entry for `cubridge` |
| `cu: command not found` (or the command opens a modem/serial session) | **macOS ships its own `/usr/bin/cu`** (the serial-port tool). Ensure the repo's `cu` comes first in `PATH` (e.g. `export PATH="$HOME/.cargo/bin:$PATH"` after `cargo build --release`), or call the full path to `target/release/cu` |
| tools fail with `CONTROL_LOCKED` — "Another client owns the active computer-use session." | another client (CLI / OpenCode MCP / Pi) created the active session. Only the owning client may stop it. The Pi extension's `COMPUTER_USE_EXISTING_SESSION_POLICY` is `reject` only — the removed `read_only`/`attach` values print a deprecation warning and behave exactly like `reject` (a token-less attach grants nothing). Re-attaching **with** the foreign session's tokens is done in the SDK (`attachReadOnly(sessionId, observationToken)` / `attachControlToken`) |
| `SESSION_NOT_FOUND` on the very first call | expected if you run `status` before any session exists — the **first `observe`/`act` auto-creates** one, so you never need an explicit start |
| `STALE_FRAME` right after switching windows / apps | acting on the last frame after the screen changed is rejected by design (strict policy, plus live visual comparison). Re-`observe` to get a current frame |
| `/computer-observe` saved screenshot "missing" from the working directory | screenshots are written to the **system temp dir** (`oc-computer-use-<session>-<frame>.jpg|png`, `0600`) and cleaned on age/session stop/extension exit — they never land in your repo |
| OpenCode: "model not found" for `glm-4.6` | the zhipuai model id for this OpenCode build is `glm-4.6v` (OpenCode 1.18 renamed vision models); update `model` in `~/.config/opencode/opencode.json` (the JSONC merge in `cu-opencode setup` never touches `model`) |
| OpenCode MCP server shows "failed" in `opencode mcp list` | `computer-use-mcp` not on the PATH OpenCode sees, or a leftover placeholder entry in `mcp` — run `cu-opencode setup` (rewrites only the `computer-use` entry), then restart OpenCode |

## Rebuilding the bridge by hand

```bash
swiftc -O -o "$HOME/.computer-use/bin/cubridge" \
  crates/cu-driver-macos/swift/CUBridge/Sources/CUBridge/main.swift \
  -framework ScreenCaptureKit -framework CoreGraphics -framework AppKit \
  -framework ApplicationServices -framework CoreImage
```

Remember: **re-grant Screen Recording afterwards.**
