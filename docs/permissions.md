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

## Rebuilding the bridge by hand

```bash
swiftc -O -o "$HOME/.computer-use/bin/cubridge" \
  crates/cu-driver-macos/swift/CUBridge/Sources/CUBridge/main.swift \
  -framework ScreenCaptureKit -framework CoreGraphics -framework AppKit \
  -framework ApplicationServices -framework CoreImage
```

Remember: **re-grant Screen Recording afterwards.**
