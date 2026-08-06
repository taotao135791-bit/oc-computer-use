# Security

The computer-use runtime is a **single-user, localhost** automation stack: a
JSON-RPC daemon over a Unix socket, a `cu` CLI, a TypeScript SDK, an MCP
server, and a Pi extension. There is no network listener; every transport is
a Unix socket owned by the logged-in user. This file states the threat model
and the concrete protections each layer provides.

## Threat model

- **Trust boundary**: the local user and their processes. Anything the local
  user can execute runs with the user's permissions anyway — the protections
  below are *defense in depth* against mistakes, other local processes, and
  accidental exposure (shell history, logs, backups, screenshots, `ps`),
  not against a malicious process running as the same user.
- **No remote surface**: the daemon binds a Unix socket with mode `0700`
  (owner only, inside a `0700` runtime directory). `runtime.shutdown`
  additionally requires a daemon admin token — a random peer on the machine
  cannot ask the daemon to stop.
- **One active session**: the runtime holds a single session (control lock).
  The session's **owner** is the client that created it; only the owner
  stops it.

## Secrets and where they live

| Secret | Created | Stored | Mode | Lifecycle |
|---|---|---|---|---|
| session `observation_token` (256-bit CSPRNG) | `session start` | SHA-256 hash in daemon memory **and** the trace access manifest (`traces/<sid>.manifest.json`, `0600`, atomically written) | — | issued exactly once; `stop` invalidates it; the hashed manifest keeps trace reads working across daemon restarts |
| session `control_token` (256-bit CSPRNG) | `session start` | SHA-256 hash in daemon memory **and** the trace access manifest | — | issued exactly once; never repeated by `status` |
| daemon admin token (256-bit CSPRNG) | daemon startup | `~/.local/state/oc-computer-use/` admin token file | `0600` | deleted on shutdown; corrupt store refuses startup |
| CLI credential files | `cu session start` | `~/.local/state/oc-computer-use/credentials/<sid>.json` | `0600` | deleted with the session |

Rules:

- **A session id alone grants nothing.** Every sensitive read requires the
  observation token (or the control token); every mutation requires the
  control token. Token errors are deliberately non-descriptive
  (`INVALID_*` never says which token was wrong).
- **Tokens are issued exactly once** and never printed by the CLI (`cu
  session start` shows them redacted); `status` never re-issues them.
- **SecretToken values redact themselves** in `Debug`/`Display`/`to_string`
  output (they print `[REDACTED]`) and zeroize their buffer on drop; trace
  recording stores only a redaction marker, never the secret.
- SDK credentials are **in-memory only**; only the CLI persists them (files
  above). A session's tokens never appear in logs, traces, or the
  `opencode.json`/Pi configs.

## Credential-file write safety (`cu` CLI)

Credential files are written atomically and defensively:

1. the target is **pre-checked for a symlink** — a symlink parked at the
   target is refused (`AlreadyExists`), never followed;
2. the write goes to a temp file created with `create_new` +
   `O_NOFOLLOW`, mode `0600`, `fsync`'d, then atomically `rename`d over the
   target (replacing any raced-in symlink itself, not its target);
3. the parent directory is `fsync`'d; failed writes remove the temp file.

Reads validate the file: it must be a **regular file** (never a symlink or
directory), owned by the current euid, mode `0600`-or-stricter, ≤ 64 KiB,
`format_version` ≤ current, with a matching session id.

## Screenshot handling

- Saved screenshots (`/computer-observe`, Pi extension) go to the **system
  temp dir** with mode `0600`, named `oc-computer-use-<session>-<frame>`,
  extension by MIME only. They are removed on age timeout, session stop, and
  extension exit. The full screenshot base64 is never logged or recorded.
- Screenshots are never committed to the repository.

## Sensitive-read protection (capability tokens)

All sensitive read-only methods (`status`, `observe`, `inspect`, trace
methods) require a verified observation/control token. The capability
matrix lives in [README.md](README.md#capability-matrix) and
[docs/protocol.md](docs/protocol.md); a matrix test suite runs against a
live daemon (`cargo test -p cu-daemon --test integration -- --ignored`).

### Trace reads are session-scoped

- Every trace read addresses exactly **one session** and requires that
  session's observation/control token: a token from session A can never list
  or read session B's trace (`INVALID_OBSERVATION_TOKEN`), and a session id
  alone grants nothing (`OBSERVATION_TOKEN_REQUIRED`).
- The **cross-session** listing (`trace.admin_list`) is daemon-manager only:
  it requires the daemon admin token, never a session capability — a client
  that legitimately reads one session's trace learns nothing about which
  other sessions ever ran.
- Access survives daemon restarts through a per-session access manifest
  (`traces/<session_id>.manifest.json`) written via the shared private-file
  API (0600, atomic, symlink-refusing) recording only **SHA-256 hashes** of
  the issued tokens — plaintext tokens never touch disk, and a missing,
  corrupt, oversized, or symlinked manifest never grants access.
- **`trace.export` is a pure read (round 7).** The runtime never accepts a
  destination path: export returns the content plus its SHA-256 over the
  wire and performs **no filesystem writes**, so an observation-capable
  caller cannot create directories, overwrite files, or follow symlinks
  anywhere through the runtime. Saving an export to a user-chosen path is
  the client's job — the CLI's `cu trace export --output <path>` refuses to
  overwrite an existing file unless `--force` is given.

## Reporting a vulnerability

This project has no private disclosure channel yet. For now, open an issue
on the repository describing the issue without including real tokens or
screenshots; capability tokens are single-use and can be revoked by stopping
the session or restarting the daemon.
