# oc-computer-use {{VERSION}}

macOS Computer Use Runtime — a single-user, localhost automation stack
(JSON-RPC daemon + `cu` CLI + TypeScript SDK + MCP server + Pi extension +
OpenCode adapter).

## Version

- **Runtime version**: {{VERSION}}
- **Protocol version**: v{{PROTOCOL_VERSION}} (JSON-RPC 2.0; the committed
  [computer-use.schema.json](https://github.com/taotao135791-bit/oc-computer-use/blob/{{TAG}}/protocol/computer-use.schema.json)
  is the single source of truth — clients are refused with
  `PROTOCOL_VERSION_MISMATCH` outside the supported bounds)
- **macOS minimum**: 14 (arm64 and x86_64; universal binaries for both)

## What's in this pre-release

| Artifact | Contents |
|---|---|
| `oc-computer-use-macos-universal.tar.gz` | universal `cu` + `cubridge` binaries (run `cu daemon run`; the daemon runs in-process — there is no separate daemon binary) |
| `computer-use-sdk-*.tgz` | TypeScript SDK (`@computer-use/sdk`) |
| `computer-use-mcp-server-*.tgz` | MCP server (`@computer-use/mcp-server`; `computer-use-mcp` bin) |
| `computer-use-pi-extension-*.tgz` | Pi extension (`@computer-use/pi-extension`) |
| `computer-use-opencode-adapter-*.tgz` | OpenCode adapter (`@computer-use/opencode-adapter`; `cu-opencode` bin) |
| `computer-use.schema.json` | protocol schema (mirrors `protocol/` at this tag) |
| `checksums.txt` | SHA-256 of every artifact |
| `CHANGELOG.md` | full changelog at this tag |

## Installation

```sh
# Runtime (universal binaries):
tar -xzf oc-computer-use-macos-universal.tar.gz
#   cu, cubridge → put both on your PATH
cu --help

# SDK / MCP / Pi / OpenCode (publish order: sdk → mcp-server → pi-extension
# → opencode-adapter):
npm install ./computer-use-sdk-<version>.tgz
npm install ./computer-use-mcp-server-<version>.tgz
npm install ./computer-use-pi-extension-<version>.tgz
npm install ./computer-use-opencode-adapter-<version>.tgz

# Start the daemon, then create a session:
cu daemon start
cu session start
```

Required permissions (first run):

1. **Screen Recording** — System Settings → Privacy & Security → Screen
   Recording → allow your terminal/app (capture fails with a permission
   error until granted).
2. **Accessibility / Automation** — System Settings → Privacy & Security →
   Accessibility → allow your terminal/app (input events and window
   inspection require it).

## Signing and Gatekeeper status (this is an unsigned pre-release)

- The binaries are **NOT signed with a Developer ID** and **NOT notarized**.
- Gatekeeper may refuse or warn on first launch (e.g. "Apple cannot check it
  for malicious software"). If it does, right-click → Open, or run
  `xattr -d com.apple.quarantine /path/to/cu` — at your own discretion.
- This is stated explicitly because unsigned binaries **must never be
  described as signed**.

## Verification status (honest)

| Item | Status |
|---|---|
| Pi host acceptance (real Pi, observe + act end-to-end) | NOT VERIFIED in this pre-release |
| OpenCode model-driven acceptance (real MCP host, model tool use) | NOT VERIFIED in this pre-release |

The runtime is verified by the automated gates (see below); the two host
acceptances above are performed on the live desktop before `0.2.0-alpha.2`
and reported in the repository at that point.

## Threat model (same-UID)

This is a **single-user, localhost** stack: every transport is a Unix socket
owned by the logged-in user. Anything that runs as the same user can
interact with a session; the capability-token design is defense in depth
against mistakes and accidental exposure (shell history, backups,
screenshots), **not** against a malicious same-UID process. See
`SECURITY.md` at this tag for the full threat model, including
`trace.export`'s pure-read semantics (the runtime never accepts a
destination path).

## Automated gates (all green at this tag)

- `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace`
- `pnpm generate:protocol` / `pnpm check:protocol` (no drift) / `pnpm -r build` / `pnpm -r typecheck` / `pnpm -r lint` / `pnpm -r test`
- Swift bridge: arm64 + x86_64 + lipo universal
- npm tarballs installed fresh and verified (MCP `.bin` shim, stdio
  initialize + tools/list, SDK/Pi/OpenCode imports)
- release build + `shasum -c checksums.txt`
