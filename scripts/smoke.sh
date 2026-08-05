#!/usr/bin/env bash
# Strict non-interactive smoke test: build everything, run every automated
# gate, and verify the npm artifacts (MCP executable, SDK/Pi/OpenCode
# imports) and release checksums end-to-end.
#
# Every gate is judged by its exit code — never by grepping output to guess
# whether it passed. A gate that prints warnings but exits 0 is green; a gate
# that exits nonzero is red. There is deliberately no "grep -q" anywhere in
# the pass/fail logic.
#
# Modes:
#   scripts/smoke.sh            full: all gates (slow: ~minutes, includes
#                               cross-arch release build + checksums and the
#                               npm tarball install tests)
#   scripts/smoke.sh --fast     skips the slow artifact gates (npm tarballs
#                               + release checksums); runs everything else
#   scripts/smoke.sh --self-test
#                               runs one deliberately-failing gate through the
#                               same aggregation path and exits 1 — proof
#                               that a failing gate fails the run
#
# Requires: cargo, rustup targets aarch64-apple-darwin + x86_64-apple-darwin
# (full mode), swiftc, pnpm. No daemon, no permissions, no display needed.
# Exit 0 = all gates green, 1 = anything failed, 2 = usage error.
set -u
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

MODE=full
case "${1:-}" in
  --fast) MODE=fast ;;
  --self-test) MODE=self-test ;;
  --help|-h)
    echo "usage: scripts/smoke.sh [--fast|--self-test]"; exit 0 ;;
  "") : ;;
  *) echo "smoke: unknown option: $1" >&2; echo "usage: scripts/smoke.sh [--fast|--self-test]" >&2; exit 2 ;;
esac

if [ ! -d node_modules ]; then
  echo "== installing dependencies (node_modules absent)"
  pnpm install || { echo "FAIL pnpm install"; exit 1; }
fi

# ---------------------------------------------------------------------------
# Self-test mode runs nothing but the deliberately-failing gate: it proves,
# in seconds, that a failing gate increments FAIL and that the run exits 1.
# ---------------------------------------------------------------------------
if [ "$MODE" = "self-test" ]; then
  step() { echo; echo "== $1 =="; }
  PASS=0; FAIL=0
  run_check() {
    local label="$1"; shift
    if "$@"; then PASS=$((PASS + 1)); echo "  ok   $label"
    else FAIL=$((FAIL + 1)); echo "  FAIL $label"; fi
  }
  step "Deliberate-false self-test (proves strict exit codes)"
  run_check "deliberate false gate" false
  echo
  echo "== smoke result (self-test): $PASS ok, $FAIL failed =="
  [ "$FAIL" -eq 0 ]
  exit $?
fi

step() { echo; echo "== $1 =="; }
PASS=0; FAIL=0

# run_check <label> <command...> — the single pass/fail path. The command's
# own exit status decides the gate; nothing is inferred from its output.
run_check() {
  local label="$1"; shift
  if "$@"; then
    PASS=$((PASS + 1)); echo "  ok   $label"
  else
    FAIL=$((FAIL + 1)); echo "  FAIL $label"
  fi
}

# ---------------------------------------------------------------------------
# Rust: format, lint (warnings are errors), full test suite.
# ---------------------------------------------------------------------------
step "Rust: fmt / clippy / tests"
run_check "cargo fmt --check" cargo fmt --check
run_check "cargo clippy (warnings are errors)" \
  cargo clippy --workspace --all-targets -- -D warnings
run_check "cargo test --workspace" cargo test --workspace

# ---------------------------------------------------------------------------
# Protocol: regenerate (proves generation runs) then drift-check (proves the
# committed artifacts are byte-identical to what the Rust types produce).
# ---------------------------------------------------------------------------
step "Protocol: generate + drift check"
run_check "pnpm generate:protocol" pnpm generate:protocol
run_check "pnpm check:protocol (no drift)" pnpm check:protocol

# ---------------------------------------------------------------------------
# TypeScript: build / typecheck / lint / test — all exit-code judged.
# ---------------------------------------------------------------------------
step "TypeScript: build / typecheck / lint / test"
run_check "pnpm -r build" pnpm -r build
run_check "pnpm -r typecheck" pnpm -r typecheck
run_check "pnpm -r lint" pnpm -r lint
run_check "pnpm -r test" pnpm -r test

# ---------------------------------------------------------------------------
# Swift bridge: compiles on both macOS architectures and lipo's into a
# universal binary containing exactly arm64 + x86_64.
# ---------------------------------------------------------------------------
SWIFT_DIR="$(mktemp -d /tmp/cu-smoke-swift.XXXXXX)"
SWIFT_SRC="crates/cu-driver-macos/swift/CUBridge/Sources/CUBridge/main.swift"
FRAMEWORKS="-framework ScreenCaptureKit -framework CoreGraphics -framework AppKit -framework ApplicationServices -framework CoreImage"
step "Swift bridge (arm64 + x86_64 + universal)"
run_check "swiftc arm64" \
  swiftc -O -target arm64-apple-macosx14.0 -o "$SWIFT_DIR/cubridge-arm64" "$SWIFT_SRC" $FRAMEWORKS
run_check "swiftc x86_64" \
  swiftc -O -target x86_64-apple-macosx14.0 -o "$SWIFT_DIR/cubridge-x86_64" "$SWIFT_SRC" $FRAMEWORKS
run_check "lipo universal" \
  lipo -create -output "$SWIFT_DIR/cubridge-universal" "$SWIFT_DIR/cubridge-arm64" "$SWIFT_DIR/cubridge-x86_64"
if lipo -info "$SWIFT_DIR/cubridge-universal" >/dev/null 2>&1 \
   && [ "$(lipo -archs "$SWIFT_DIR/cubridge-universal" | tr ' ' '\n' | sort | tr '\n' ' ')" = "arm64 x86_64 " ]; then
  run_check "universal binary holds arm64 + x86_64" true
else
  run_check "universal binary holds arm64 + x86_64" false
fi
rm -rf "$SWIFT_DIR"

# ---------------------------------------------------------------------------
# CLI: help paths exit 0 and never block (a regression here previously
# wedged on stdio).
# ---------------------------------------------------------------------------
step "CLI / daemon help (exit 0, no blocking)"
run_check "cargo build -p cu-cli" cargo build -p cu-cli
run_check "cu --help" ./target/debug/cu --help
run_check "cu daemon --help" ./target/debug/cu daemon --help
run_check "cu daemon run --help" ./target/debug/cu daemon run --help
run_check "cu trace --help" ./target/debug/cu trace --help
run_check "cu session --help" ./target/debug/cu session --help

# ---------------------------------------------------------------------------
# npm artifacts (full mode only): pack every package, install the tarballs
# into a clean directory in dependency order, and verify — with real exit
# codes — the MCP bin is executable with a shebang, answers --help, and
# serves tools/list over stdio; the SDK ESM import + types resolve; the Pi
# factory registers its tools; the OpenCode adapter config + cli.js work;
# and no installed artifact references the repository path.
# ---------------------------------------------------------------------------
if [ "$MODE" = "full" ]; then
  step "npm tarballs: install + executable + stdio + imports (verify-tarballs.mjs)"
  run_check "verify-tarballs.mjs (22 assertions)" node scripts/verify-tarballs.mjs
else
  echo "== npm tarball gates skipped (--fast)"
fi

# ---------------------------------------------------------------------------
# Release artifacts (full mode only): the release build script produces the
# per-arch + universal binaries, npm tarballs, and checksums.txt; shasum -c
# then proves every listed artifact hashes to its recorded value.
# ---------------------------------------------------------------------------
if [ "$MODE" = "full" ]; then
  step "Release build + checksum verification"
  run_check "scripts/release-build.sh" bash scripts/release-build.sh
  run_check "shasum -c dist/checksums.txt (every artifact verified)" \
    bash -c 'cd dist && shasum -a 256 -c checksums.txt'
else
  echo "== release artifact gates skipped (--fast)"
fi

echo
echo "== smoke result ($MODE): $PASS ok, $FAIL failed =="
[ "$FAIL" -eq 0 ]
