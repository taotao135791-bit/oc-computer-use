# Uninstall / cleanup

## 1. Stop the daemon

```bash
cu daemon stop        # graceful shutdown; removes the socket
```

If the socket is stale or `cu` is missing, remove it by hand:
`rm -f ~/.computer-use/runtime.sock`

## 2. Remove runtime state

```bash
rm -rf ~/.computer-use        # socket, frames, traces, daemon.log, bin/cubridge
```

`COMPUTER_USE_HOME` overrides this directory — check
`echo ${COMPUTER_USE_HOME:-~/.computer-use}`.

## 3. Revoke macOS permissions (recommended)

The TCC entries for `cubridge` remain after deletion:

```bash
tccutil reset ScreenCapture  "$HOME/.computer-use/bin/cubridge" 2>/dev/null || \
  tccutil reset ScreenCapture
tccutil reset Accessibility  "$HOME/.computer-use/bin/cubridge" 2>/dev/null || true
```

Or remove the entries by hand in System Settings → Privacy & Security.

## 4. Remove the package

```bash
pnpm -r remove 2>/dev/null; pnpm install  # if installed via the workspace
cargo clean                               # build artifacts
```

No launch agents or system services are installed — the daemon is a plain
background process and leaves nothing behind when stopped.
