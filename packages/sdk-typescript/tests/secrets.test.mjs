// redactSecrets: the TS half of the secret-redaction rule (the Rust half is
// cu_core::security::redact_json, exercised in crates/cu-core). Both must
// agree: a field is a secret when its lowercased key contains "token" or
// "secret"; string values become "[REDACTED]"; the input is never mutated.
import { test } from "node:test";
import assert from "node:assert/strict";

import { redactSecrets } from "../dist/index.js";

test("redactSecrets replaces token fields at any depth", () => {
  const payload = {
    jsonrpc: "2.0",
    method: "computer.act",
    params: {
      session_id: "s1",
      control_token: "plaintext-control",
      actions: [{ type: "wait", duration_ms: 1 }],
    },
    nested: { observation_token: "plaintext-obs", keep: "visible" },
    list: [{ admin_token: "plaintext-admin" }],
  };
  const r = redactSecrets(payload);
  assert.equal(r.params.control_token, "[REDACTED]");
  assert.equal(r.nested.observation_token, "[REDACTED]");
  assert.equal(r.list[0].admin_token, "[REDACTED]");
  assert.equal(r.params.session_id, "s1", "non-secret fields pass through");
  assert.equal(r.nested.keep, "visible");
  assert.equal(r.method, "computer.act");
  // The input is untouched — redaction returns a copy.
  assert.equal(payload.params.control_token, "plaintext-control");
});

test("redactSecrets mirrors the Rust rule for edge cases", () => {
  // Non-string values under secret keys pass through.
  assert.equal(redactSecrets({ control_token: 42 }).control_token, 42);
  // Case-insensitive key match (ADMIN_TOKEN from an echo).
  assert.equal(redactSecrets({ ADMIN_TOKEN: "x" }).ADMIN_TOKEN, "[REDACTED]");
  // Null and primitives are untouched.
  assert.deepEqual(redactSecrets({ screen_token: null }), { screen_token: null });
  assert.equal(redactSecrets("plain"), "plain");
  assert.equal(redactSecrets(7), 7);
  assert.equal(redactSecrets(null), null);
});
