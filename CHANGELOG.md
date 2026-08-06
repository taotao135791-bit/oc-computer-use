# Changelog

## 0.2.0-alpha.2 (2026-08-06) — release hardening + desktop benchmark

### Security

- **`trace.export` is a pure read.** The runtime never accepts a destination
  path: `trace.export` returns the content plus its SHA-256 over the wire and
  performs **no filesystem writes** — an observation-capable caller can no
  longer create directories, overwrite files, or follow symlinks anywhere
  through the runtime. Saving an export to a user-chosen path is the CLI's
  job (`cu trace export --output <path>`), which refuses to overwrite an
  existing file unless `--force` is given.
- **Trace access manifests now actually persist across restarts.** The
  traces directory is forced `0700` when the recorder opens a session
  (`create_dir_all` previously left the umask's world-readable bits, so the
  manifest writer — which refuses directories with any group/other bits —
  always failed silently and every historical trace was unreadable after a
  daemon restart). Regression-tested.

### Observability

- **`cu trace analyze <session-id> [--json]`**: per-session metrics (events,
  actions by type, observe/screenshot bytes, stale rejections, cancellations,
  timeouts, failure category from the documented taxonomy) plus a compact
  timeline — reads through the pure `trace.export` path with the observation
  credential.
- **Richer trace events**: `act.stale_rejected` records the rejected frame
  id and change score; `cancel` records scope and request id; `observe`
  records `screenshot_bytes`; failed actions record their error detail.
- Failure classification matches the runtime's real timeout wording
  ("request timed out: …") in both the CLI and the benchmark runner.

### Release

- **Tag-triggered GitHub pre-release workflow** (`v*` tags): full gates →
  dual-arch release build → universal tarball → protocol schema → checksums
  → `gh release create --prerelease` with generated notes. All gates run
  inside the workflow (smoke, cargo fmt/clippy/test, pnpm gates), so a
  release that fails any gate is never created.
- **`benchmarks/`**: repeatable macOS desktop benchmark — 30 tasks
  (TextEdit / Finder / System Settings / Calculator / Safari / cross-app),
  declarative evaluators only (filesystem, `defaults`, HTTP fixture, app
  running state, human review), `cu-bench` runner (list / run / report /
  compare / replay), local fixture web app, deterministic seed/cleanup
  scripts. See [benchmarks/README.md](benchmarks/README.md).

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
- **`runtime.version`** additionally reports `daemon_instance_id`: a fresh
  random id each daemon start. The CLI verifies it before using a persisted
  admin credential, so a stale credential from another install can never shut
  a running daemon down.
- **Trace reads are session-scoped.** `trace.list` / `trace.summaries`
  address exactly one `session_id` and require that session's
  observation/control token — a token from another session is
  `INVALID_OBSERVATION_TOKEN`. The cross-session listing is `trace.admin_list`
  (daemon-manager only: admin token, `DAEMON_ADMIN_TOKEN_REQUIRED` /
  `INVALID_DAEMON_ADMIN_TOKEN` like `runtime.shutdown`). A session capability
  never reveals which other sessions ran.
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
- **Credential-file write safety** (shared `private_file` module): daemon
  admin credentials and CLI session credentials are all written through one
  implementation — saves go through `create_new` + `O_NOFOLLOW` + mode
  `0600` + `fsync` + atomic `rename` + parent-directory `fsync`, with failure
  cleanup; a symlink parked at the target is refused (`AlreadyExists`).
  Reads validate: regular file (never a symlink), owner uid, mode ≤ 0600,
  size ≤ 64 KiB, `format_version` ≤ 1, matching session id.
- **Trace access manifests**: `traces/<session_id>.manifest.json` records
  only the SHA-256 hashes of a session's tokens (through the same
  `private_file` API) so trace reads keep working after a daemon restart;
  `stopped_at` is stamped on session stop. A missing, corrupt, oversized, or
  symlinked manifest never grants access.
- **Strict smoke test** (`scripts/smoke.sh`): every gate is judged by its
  exit code (no grep-guessing); `--fast` skips only the slow artifact gates;
  `--self-test` proves a failing gate fails the run (exit 1); usage errors
  exit 2. Full mode additionally verifies every npm tarball in a fresh
  `npm install` (MCP bin executable + shebang, SDK/Pi/OpenCode imports) and
  the release checksums end-to-end (`shasum -a 256 -c`).
- **MCP server is an npm-executable package**: the `computer-use-mcp` bin is
  a real shebang'd script (`#!/usr/bin/env node`, mode 0755, packaged),
  verified by installing the packed tarball fresh.
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
- `cu trace list` moved to `trace.admin_list` (daemon-manager role) — a
  session credential can no longer list every session's traces.

### Security

- Daemon admin and session credentials are unified on one defensively
  written private-file implementation (atomic, symlink-refusing, validated
  reads) — no file is ever written via a plain `fs::write` with attacker
  control over the path.
- A stale admin credential is never used: `cu daemon stop` proves the
  running daemon is the instance that wrote the credential
  (`daemon_instance_id` in `runtime.version`) before shutting it down.
- Trace access is bound to one session plus a persisted, hashed-only access
  manifest — cross-session trace disclosure after a restart is impossible
  without the original tokens.

## Known limitations (0.2.0-alpha.1)

- **Pi host rendering**: in-Pi-app image rendering is untestable without a
  Pi host; the extension is verified in a host shim against the real
  daemon/screen.
- **OpenCode model-driven steps**: zhipuai GLM Coding Plan on the acceptance
  machine expired during round 2 (account issue); model-call steps are
  NOT VERIFIED, tool-level steps are.
- **Code signing / notarization**: binaries are unsigned — macOS may prompt
  on first launch; Gatekeeper quarantine applies to downloaded builds.
- **Same-UID trust boundary**: any process running as your user can talk to
  the socket (mode 0700) — the protections are defense-in-depth against
  mistakes, not against a malicious same-user process.
- **Single-session runtime**: the control lock allows one active session at
  a time; read-only observers attach with the session's observation token.

## 0.1.0 (2026-08-04) — initial runtime

First released baseline: JSON-RPC daemon over a Unix socket, `cu` CLI,
sessions with ownership + control lock, stale-frame protection, pause /
takeover / release, trace recording with redaction, Swift ScreenCaptureKit
bridge, TypeScript SDK, MCP server, Pi extension, OpenCode adapter, local
inspector, manual + automated acceptance.
