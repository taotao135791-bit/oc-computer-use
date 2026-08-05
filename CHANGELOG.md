# Changelog

## 0.2.0-alpha.1 (2026-08-05) — protocol v3: capability tokens

### Breaking changes

- **Capability tokens.** `session start` now issues **two** 256-bit tokens
  exactly once: an `observation_token` (sensitive reads: `status`, `observe`,
  `inspect`, trace methods) and a `control_token` (mutating operations, which
  also opens reads). The daemon stores only SHA-256 hashes and never repeats
  the tokens — `status` no longer returns them. **Knowing a session ID grants
  no observation or control permission.**
  - Reads without a token: `OBSERVATION_TOKEN_REQUIRED` (-32024); a wrong
    token: `INVALID_OBSERVATION_TOKEN` (-32025). Mutations without a token:
    `CONTROL_TOKEN_REQUIRED` (-32019); wrong: `INVALID_CONTROL_TOKEN`
    (-32020). Token errors are deliberately non-descriptive.
- **`runtime.shutdown` requires a daemon admin token** (256-bit CSPRNG,
  persisted `0600` at startup, `DAEMON_ADMIN_TOKEN_REQUIRED` -32026 /
  `INVALID_DAEMON_ADMIN_TOKEN` -32027). A corrupt admin-token store refuses
  startup rather than leaving the daemon unstoppable.
- **SDK credentials are in-memory only.** `SessionCredential` is now
  `{sessionId, observationToken, controlToken?, ownerClientId?,
  ownerInstanceId?, access: "read_only" | "control"}`. Only the CLI (`cu`)
  persists credentials, to files with mode `0600`.
- **`COMPUTER_USE_EXISTING_SESSION_POLICY=attach` is deprecated** → maps to
  `read_only` with a warning. A tokenless `read_only` attach is refused with
  `INVALID_PARAMS` + an `attachReadOnly` hint (the daemon refuses tokenless
  reads; there is no silent observation anymore).
- **`runtime.version`** now reports
  `{runtime_version, protocol_version, minimum_client_protocol_version,
  maximum_client_protocol_version}`; a client outside the bounds is refused
  with `PROTOCOL_VERSION_MISMATCH` (-32023).
- **Protocol is a single source of truth**: Rust wire types → schemars JSON
  Schema → generated TypeScript bindings. `pnpm check:protocol` fails the
  build on any drift.

### Added

- CI (`.github/workflows/ci.yml`): Rust fmt/clippy/test, TypeScript
  lint/typecheck/build/test, protocol-drift, secret scan (gitleaks),
  Swift bridge arm64+x86_64+universal, package smoke.
- Secret scanning with gitleaks (field-anchored regexes for capability
  tokens, no entropy threshold that would misfire on real tokens).
- Full error table (incl. -32600 `INVALID_REQUEST`, -32024/-32025/-32026/
  -32027) and a capability matrix in `docs/protocol.md` / `README.md`.

### Fixed

- Pi extension `COMPUTER_USE_EXISTING_SESSION_POLICY` never accepted
  `read_only` (only the legacy `attach`) — both now work.
- Acceptance scripts reworked for v3 (state read through the client that owns
  the session; orphaned-process cleanup; explicit exit so piped runs finish).
- Acceptance recording in `docs/acceptance-manual.md` (round 4): Pi 32/32
  PASS, OpenCode/MCP 17/17 PASS, `opencode mcp list` → `computer-use`
  connected.

## 0.1.0 (2026-08-04) — initial runtime

First released baseline: JSON-RPC daemon over a Unix socket, `cu` CLI,
sessions with ownership + control lock, stale-frame protection, pause /
takeover / release, trace recording with redaction, Swift ScreenCaptureKit
bridge, TypeScript SDK, MCP server, Pi extension, OpenCode adapter, local
inspector, manual + automated acceptance.
