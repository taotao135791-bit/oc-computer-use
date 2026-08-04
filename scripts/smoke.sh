#!/usr/bin/env bash
# Non-interactive smoke test: build everything, run every automated gate, and
# snapshot the two agent integrations (Pi tool registration + OpenCode MCP
# config) so regressions in the wiring show up without a GUI session.
#
# Requires: cargo, pnpm. No daemon, no permissions, no display needed.
# Exit 0 = all gates green, 1 = anything failed.
set -u
cd "$(dirname "$0")/.."
export PATH="$HOME/.cargo/bin:$PATH"

FAIL=0
step() { echo; echo "== $1 =="; }
ok()   { echo "  ok   $*"; }
bad()  { echo "  FAIL $*"; FAIL=$((FAIL + 1)); }

step "Rust: fmt / clippy / tests"
cargo fmt --check || bad "cargo fmt --check"
cargo clippy --workspace --all-targets 2>&1 | grep -qE "^(warning|error)" && bad "clippy warnings" || ok "clippy clean"
RUST_TEST=$(cargo test --workspace 2>&1)
echo "$RUST_TEST" | grep -q "FAILED" && bad "cargo test failures" || ok "cargo test passed"
echo "$RUST_TEST" | grep "test result" | awk -F'[ ;]' '{p+=$4} END {printf "        (%d tests passed)\n", p}'

step "TypeScript: build / typecheck / lint / test"
pnpm -r build >/dev/null 2>&1 || bad "pnpm build"
pnpm -r typecheck 2>&1 | grep -qE "error TS" && bad "typecheck errors" || ok "typecheck clean"
pnpm -r lint 2>&1 | grep -qE "error TS" && bad "lint errors" || ok "lint clean"
TS_TEST=$(pnpm -r test 2>&1)
echo "$TS_TEST" | grep -qE "✖" && bad "ts test failures" || ok "ts tests passed"
echo "$TS_TEST" | grep -E "ℹ pass [0-9]+" | awk '{s+=$NF} END {printf "        (%d tests passed)\n", s}'

step "Pi extension: tool-registration snapshot"
node --input-type=module -e '
  const { default: ext } = await import("./packages/pi-extension/dist/index.js");
  if (typeof ext !== "function") throw new Error("default export is not a factory");
  const tools = [], commands = [], handlers = {};
  ext({
    registerTool: (d) => tools.push(d.name),
    registerCommand: (n) => commands.push(n),
    on: (e, h) => { handlers[e] = h; },
  });
  const expectedTools = ["computer_session", "computer_observe", "computer_act", "computer_inspect"];
  const missing = expectedTools.filter((t) => !tools.includes(t));
  if (missing.length) throw new Error("missing tools: " + missing);
  if (commands.length < 7) throw new Error("expected >=7 commands, got " + commands.length);
  if (typeof handlers.session_shutdown !== "function") throw new Error("no session_shutdown handler");
  console.log("  4 tools registered: " + expectedTools.join(", "));
  console.log("  " + commands.length + " commands registered; session_shutdown handler present");
' && ok "pi registration snapshot" || bad "pi registration snapshot"

step "OpenCode: official MCP config snapshot"
node --input-type=module -e '
  const { generateOpenCodeConfig } = await import("./packages/opencode-adapter/dist/index.js");
  const cfg = generateOpenCodeConfig();
  const entry = cfg.mcp["computer-use"];
  const want = { type: "local", command: ["computer-use-mcp"], enabled: true };
  if (JSON.stringify(entry) !== JSON.stringify(want)) {
    throw new Error("unexpected mcp entry: " + JSON.stringify(entry));
  }
  console.log("  mcp.computer-use = " + JSON.stringify(entry));
' && ok "opencode mcp config snapshot" || bad "opencode mcp config snapshot"

echo
echo "== smoke result: $FAIL failed =="
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
