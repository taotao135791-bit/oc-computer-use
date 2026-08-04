#!/usr/bin/env bash
# Manual acceptance script for the computer-use runtime.
#
# Run from the repo root. Most steps need a logged-in GUI session and the
# Screen Recording + Accessibility permissions granted (see docs/permissions.md).
# This script drives the daemon through every security check; it is interactive
# by design — read each step, run it, confirm the expected result.
#
# Exit codes: 0 = fully passed, 1 = one or more steps failed.

set -u
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

CU=${CU:-"$PWD/target/debug/cu"}
if [ ! -x "$CU" ]; then
  echo "building cu-cli first…" >&2
  cargo build -p cu-cli >/dev/null 2>&1 || { echo "build failed"; exit 1; }
fi

PASS=0
FAIL=0

check() { # check <name> <expected-substring> <actual-output>
  local name="$1" needle="$2" out="$3"
  if printf '%s' "$out" | grep -qF "$needle"; then
    echo "  ok   $name"
    PASS=$((PASS + 1))
  else
    echo "  FAIL $name — expected to contain: $needle"
    printf '%s\n' "$out" | sed 's/^/       /' | head -5
    FAIL=$((FAIL + 1))
  fi
}

die() { echo "ABORT: $*" >&2; exit 1; }

echo "== computer-use manual acceptance =="
# Stop any session left over from a previous run (not an error if none).
LEFT=$("$CU" session status --json 2>/dev/null | grep -oE '"session_id": "s_[a-f0-9]+"' | cut -d'"' -f4)
if [ -n "$LEFT" ]; then
  "$CU" session stop --session-id "$LEFT" >/dev/null 2>&1 || true
fi

echo "ensure the daemon is running:"
"$CU" daemon start 2>/dev/null || true
"$CU" daemon status || die "daemon did not start"

echo
echo "-- 1. doctor reports healthy ------------------------------------------------"
check "doctor" "[ ok ]" "$("$CU" doctor 2>&1)"

echo "-- 2. permissions reported with guidance ------------------------------------"
check "permissions" '"screen_recording"' "$("$CU" permissions 2>&1)"

echo "-- 3. displays are listed ---------------------------------------------------"
check "displays" '"bounds"' "$("$CU" displays 2>&1)"

echo "-- 4. session start binds a display -----------------------------------------"
SESSION_JSON=$("$CU" session start --json 2>&1) || die "session start failed: $SESSION_JSON"
check "session.start" '"session_id"' "$SESSION_JSON"
SESSION_ID=$(printf '%s' "$SESSION_JSON" | grep -oE '"session_id": "s_[a-f0-9]+"' | cut -d'"' -f4)
[ -n "$SESSION_ID" ] || die "no session_id from start"
check "session.id-nonempty" "$SESSION_ID" "$SESSION_JSON"

echo "-- 5. observe returns a frame ------------------------------------------------"
OBSERVE=$("$CU" observe --session-id "$SESSION_ID" --include-image --json 2>&1)
check "observe.frame_id" '"frame_id"' "$OBSERVE"
FRAME_ID=$(printf '%s' "$OBSERVE" | grep -oE '"frame_id": "frame_[0-9]+"' | cut -d'"' -f4 | head -1)
[ -n "$FRAME_ID" ] || die "no frame_id from observe"
check "observe.image" '"image_base64"' "$OBSERVE"

echo "-- 6. click executes (top-left 10% of screen, safe-ish spot) -----------------"
check "click" '"status": "success"' "$("$CU" click 100 100 --session-id "$SESSION_ID" --frame-id "$FRAME_ID" --json 2>&1)"

echo "-- 7. move executes ----------------------------------------------------------"
check "move" '"status": "success"' "$("$CU" move 120 120 --session-id "$SESSION_ID" --json 2>&1)"

echo "-- 8. type is executed but REDACTED in traces --------------------------------"
check "type" '"status": "success"' "$("$CU" type 'secret-password-123' --session-id "$SESSION_ID" --json 2>&1)"
TRACE=$("$CU" trace get "$SESSION_ID" 2>&1)
if printf '%s' "$TRACE" | grep -qF 'secret-password-123'; then
  echo "  FAIL trace redaction — plaintext leaked!"; FAIL=$((FAIL + 1))
else
  echo "  ok   trace redaction"; PASS=$((PASS + 1))
fi
check "redaction.marker" 'text_redacted' "$TRACE"

echo "-- 9. stale frame is rejected (observe first, then change the screen) --------"
# FRAME_ID was captured before the app switch below. Staleness is decided by
# the live screen (plus the app identity), so a real screen change is needed
# for a deterministic rejection — an app switch is always stale by policy.
open -a TextEdit
sleep 1
check "stale-frame" 'STALE_FRAME' "$("$CU" click 100 100 --session-id "$SESSION_ID" --frame-id "$FRAME_ID" --json 2>&1)"

echo "-- 10. out-of-bounds coordinates are rejected --------------------------------"
check "oob" 'OUT_OF_BOUNDS' "$("$CU" move 9999 9999 --session-id "$SESSION_ID" --json 2>&1)"

echo "-- 11. pause rejects actions with PAUSED -------------------------------------"
"$CU" session pause --session-id "$SESSION_ID" >/dev/null 2>&1
check "paused" 'PAUSED' "$("$CU" move 100 100 --session-id "$SESSION_ID" --json 2>&1)"
"$CU" session resume --session-id "$SESSION_ID" >/dev/null 2>&1
check "resume" 'active' "$("$CU" session status --json 2>&1)"

echo "-- 12. user takeover rejects with USER_TAKEOVER ------------------------------"
"$CU" session takeover --session-id "$SESSION_ID" >/dev/null 2>&1
check "takeover" 'USER_TAKEOVER' "$("$CU" move 100 100 --session-id "$SESSION_ID" --json 2>&1)"
check "takeover-status" '"user_takeover": true' "$("$CU" session status --json 2>&1)"
"$CU" session release --session-id "$SESSION_ID" >/dev/null 2>&1

echo "-- 13. stop rejects with SESSION_STOPPED -------------------------------------"
"$CU" session stop --session-id "$SESSION_ID" >/dev/null 2>&1
check "stopped" 'SESSION_STOPPED' "$("$CU" move 100 100 --session-id "$SESSION_ID" --json 2>&1)"

echo "-- 13b. session ownership: token, owner, control lock (P0) -------------------"
# A fresh session to own.
OWN_JSON=$("$CU" session start --json 2>&1)
OWN_SID=$(printf '%s' "$OWN_JSON" | grep -oE '"session_id": "s_[a-f0-9]+"' | cut -d'"' -f4)
[ -n "$OWN_SID" ] || die "ownership: session start failed: $OWN_JSON"
check "ownership.start-token-redacted" '"control_token": "<redacted>"' "$OWN_JSON"
check "ownership.owner" '"owner_client_id": "cu-cli"' "$OWN_JSON"
# Status (read-only) never repeats the token.
OWN_STATUS=$("$CU" session status --json 2>&1)
if printf '%s' "$OWN_STATUS" | grep -qF '"control_token"'; then
  echo "  FAIL ownership — status leaked a control token"; FAIL=$((FAIL + 1))
else
  echo "  ok   ownership — status carries no control token"; PASS=$((PASS + 1))
fi
# A second start is refused while the control lock is held.
check "ownership.lock" 'CONTROL_LOCKED' "$("$CU" session start 2>&1)"
# A mutating call with a wrong token is refused with no side effects.
WRONG_STOP=$(node -e '
const net = require("net");
const sid = process.argv[1];
const sock = net.connect(process.env.HOME + "/.computer-use/runtime.sock");
let buf = "";
sock.on("data", (c) => (buf += c));
sock.on("close", () => process.stdout.write(buf.trim()));
sock.write(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "computer.session", params: { action: "stop", session_id: sid, control_token: "wrong" } }) + "\n");
setTimeout(() => sock.end(), 500);
' "$OWN_SID" 2>&1)
check "ownership.wrong-token" 'INVALID_CONTROL_TOKEN' "$WRONG_STOP"
check "ownership.survived" '"state": "active"' "$("$CU" session status --json 2>&1)"
# The credential file is 0600 and dies with the session.
OWN_CRED="$HOME/.local/state/oc-computer-use/credentials/$OWN_SID.json"
if [ -f "$OWN_CRED" ]; then
  CRED_MODE=$(stat -f '%Lp' "$OWN_CRED")
  if [ "$CRED_MODE" = "600" ]; then echo "  ok   ownership — credential file 0600"; PASS=$((PASS + 1)); else echo "  FAIL ownership — credential mode $CRED_MODE"; FAIL=$((FAIL + 1)); fi
else
  echo "  FAIL ownership — no credential file for $OWN_SID"; FAIL=$((FAIL + 1))
fi
"$CU" session stop >/dev/null 2>&1
if [ -f "$OWN_CRED" ]; then echo "  FAIL ownership — credential survives stop"; FAIL=$((FAIL + 1)); else echo "  ok   ownership — credential removed on stop"; PASS=$((PASS + 1)); fi

echo "-- 14. trace list / export / replay -------------------------------------------"
check "trace.list" 'entries' "$("$CU" trace list 2>&1)"
TMPEXPORT=$(mktemp /tmp/cu-trace-XXXX.jsonl)
check "trace.export" 'exported' "$("$CU" trace export "$SESSION_ID" "$TMPEXPORT" 2>&1)"
[ -s "$TMPEXPORT" ] && { echo "  ok   export file non-empty"; PASS=$((PASS + 1)); } || { echo "  FAIL export file empty"; FAIL=$((FAIL + 1)); }
rm -f "$TMPEXPORT"
check "trace.replay" 'replay:' "$("$CU" trace replay "$SESSION_ID" 2>&1 | head -3)"

echo "-- 15. socket is current-user-only --------------------------------------------"
SOCKMODE=$(stat -f '%Lp' "$HOME/.computer-use/runtime.sock" 2>/dev/null || echo "missing")
if [ "$SOCKMODE" = "700" ]; then echo "  ok   socket mode 0700"; PASS=$((PASS + 1)); else echo "  FAIL socket mode is $SOCKMODE (want 700)"; FAIL=$((FAIL + 1)); fi

echo "-- 16. stale socket file is replaced on startup -------------------------------"
# kill the daemon, then start it again — a leftover socket must not block it.
"$CU" daemon stop >/dev/null 2>&1 || true
sleep 1
"$CU" daemon start >/dev/null 2>&1 || die "daemon failed to (re)start"
check "daemon.restarted" 'running' "$("$CU" daemon status 2>&1)"

echo "-- 17. inspector serves the dashboard (optional, if built) --------------------"
if [ -d apps/cu-inspector ]; then
  (
    cd apps/cu-inspector
    exec env COMPUTER_USE_SOCKET="$HOME/.computer-use/runtime.sock" CU_INSPECTOR_PORT=8420 \
      node server.mjs >/dev/null 2>&1
  ) &
  INSP_PID=$!   # the subshell is the background job; exec replaces it with node
  sleep 2
  check "inspector.health" 'version' "$(curl -s --max-time 5 http://127.0.0.1:8420/api/health 2>&1)"
  check "inspector.html" 'computer-use inspector' "$(curl -s --max-time 5 http://127.0.0.1:8420/ 2>&1)"
  kill "$INSP_PID" 2>/dev/null
fi

echo "-- 18. shutdown is graceful ----------------------------------------------------"
check "shutdown" 'daemon stopped' "$("$CU" daemon stop 2>&1)"
sleep 1
"$CU" daemon start >/dev/null 2>&1  # bring it back up for further use

echo
echo "== acceptance result: $PASS passed, $FAIL failed =="
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
