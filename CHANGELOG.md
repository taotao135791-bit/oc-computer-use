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
- **`COMPUTER_USE_EXISTING_SESSION_POLICY` is `reject` only.** The pre-0.3
  `read_only`/`attach` values are **removed**: they print a deprecation
  warning and behave exactly like `reject` (`CONTROL_LOCKED`). A token-less
  attach granted nothing under v3 anyway (a session id alone holds no
  observation/control permission), so the removed behavior was silently
  powerless — refusing is honest. Re-attaching *with* the foreign session's
  tokens is an SDK operation (`attachReadOnly(sessionId, observationToken)`
  / `attachControlToken`), never an env-var policy.
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
- Full error table (incl. -32600 `INVALID_REQUEST`, -32024..-32028) and a
  capability matrix in `docs/protocol.md` / `README.md`.
- **Credential-file write safety** (`cu` CLI): saves go through
  `create_new` + `O_NOFOLLOW` + mode `0600` + `fsync` + atomic `rename` +
  parent-directory `fsync`, with failure cleanup; a symlink parked at the
  target is refused (`AlreadyExists`). Reads validate: regular file (never a
  symlink), owner uid, mode ≤ 0600, size ≤ 64 KiB, `format_version` ≤ 1,
  matching session id.
- **Graceful shutdown**: `runtime.shutdown` first marks the daemon
  shutting-down (new requests → `DAEMON_SHUTTING_DOWN`, -32028), then stops
  each session (in-flight actions return `cancelled`/`executed: false`, never
  a JSON-RPC error), shuts the driver down, drains connections within a grace
  period (default 10 s, `shutdown_grace_secs`), aborts stragglers, and
  removes the socket + admin token so a restart is clean.

### Fixed

- Pi extension `COMPUTER_USE_EXISTING_SESSION_POLICY` never accepted
  `read_only` (only the legacy `attach`); the follow-up removed both values —
  only `reject` remains, with deprecation warnings (see Breaking changes).
- Acceptance scripts reworked for v3 (state read through the client that owns
  the session; orphaned-process cleanup; explicit exit so piped runs finish).
- Acceptance recording in `docs/acceptance-manual.md` (rounds 4-5): Pi 32/32
  PASS, OpenCode/MCP 17/17 PASS, graceful shutdown verified live.

## 0.1.0 (2026-08-04) — initial runtime

First released baseline: JSON-RPC daemon over a Unix socket, `cu` CLI,
sessions with ownership + control lock, stale-frame protection, pause /
takeover / release, trace recording with redaction, Swift ScreenCaptureKit
bridge, TypeScript SDK, MCP server, Pi extension, OpenCode adapter, local
inspector, manual + automated acceptance.
