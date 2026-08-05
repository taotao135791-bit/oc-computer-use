// Protocol conformance: TypeScript-side payloads must pass the JSON Schema
// generated from the Rust wire types (the single source of truth). This is the
// TS half of the drift protection — the Rust half lives in
// crates/cu-protocol-gen/tests/schema_validation.rs; `pnpm check:protocol`
// guards the generated artifacts themselves.
//
// Runs against dist/ build products (like every SDK test): rebuild with
// `npm run build` in this package before running.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import assert from "node:assert/strict";

import Ajv2019 from "ajv/dist/2019.js";
import {
  PROTOCOL_VERSION,
  MINIMUM_CLIENT_PROTOCOL_VERSION,
  MAXIMUM_CLIENT_PROTOCOL_VERSION,
  JSONRPC_VERSION,
} from "../dist/generated/protocol.js";

const SCHEMA_PATH = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "..",
  "protocol",
  "computer-use.schema.json",
);

const schema = JSON.parse(readFileSync(SCHEMA_PATH, "utf8"));
const ajv = new Ajv2019({ strict: false, allErrors: true });

// Validate an instance against one $defs entry.
function validate(defName, instance) {
  const validateFn = ajv.compile({
    $schema: "https://json-schema.org/draft/2019-09/schema",
    $ref: `#/$defs/${defName}`,
    $defs: schema.$defs,
  });
  const ok = validateFn(instance);
  if (!ok) throw new Error(`${defName} rejected: ${JSON.stringify(validateFn.errors)}`);
}

test("generated protocol constants match the schema meta", () => {
  const meta = schema["x-protocol-meta"];
  assert.equal(PROTOCOL_VERSION, meta.protocol_version);
  assert.equal(MINIMUM_CLIENT_PROTOCOL_VERSION, meta.minimum_client_protocol_version);
  assert.equal(MAXIMUM_CLIENT_PROTOCOL_VERSION, meta.maximum_client_protocol_version);
  assert.equal(JSONRPC_VERSION, meta.jsonrpc_version);
  assert.equal(PROTOCOL_VERSION, 3);
});

test("session start response with both capability tokens passes the schema", () => {
  validate("SessionResult", {
    session_id: "s1",
    state: "active",
    paused: false,
    user_takeover: false,
    lock_held: true,
    display_id: "1",
    created_at: "2026-08-03T00:00:00Z",
    current_frame_id: "frame_9",
    trace_dir: "/tmp/cu-traces/s1",
    started_by: "pi-extension",
    control_token: "ts-fake-control-token",
    observation_token: "ts-fake-observation-token",
    owner_client_id: "client-1",
    owner_client_name: "Pi",
    owner_instance_id: "instance-1",
  });
  // The daemon issues both tokens exactly once; `status` responses without
  // them are equally schema-valid.
  validate("SessionResult", {
    session_id: "s1",
    state: "active",
    paused: false,
    user_takeover: false,
    lock_held: true,
    display_id: "1",
    created_at: "2026-08-03T00:00:00Z",
    started_by: "pi-extension",
    message: "status never repeats the capability tokens",
  });
});

test("act params pass the schema; control_token stays optional on the wire", () => {
  validate("ActParams", {
    session_id: "s1",
    frame_id: "frame_9",
    actions: [
      { type: "click", x: 100, y: 200, button: "left", coordinate_space: "normalized_1000" },
      { type: "drag", from: { x: 1, y: 2 }, to: { x: 3, y: 4 }, coordinate_space: "image_pixels" },
      { type: "type", text: "hello", method: "clipboard" },
    ],
    wait_policy: "until_stable",
    return_screenshot: true,
    control_token: "ts-fake-control-token",
  });
  // A tokenless batch is schema-valid (the daemon rejects it at runtime with
  // CONTROL_TOKEN_REQUIRED — enforcement is the daemon's, not the schema's).
  validate("ActParams", {
    session_id: "s1",
    frame_id: "frame_9",
    actions: [{ type: "wait", duration_ms: 100 }],
  });
});

test("session summary carries explicit nulls that are required", () => {
  validate("SessionSummary", {
    session_id: null,
    state: null,
    lock_held: false,
    owner_client_id: null,
    owner_client_name: null,
    message: null,
  });
  validate("SessionSummary", {
    session_id: "s1",
    state: "active",
    lock_held: true,
    owner_client_id: "c1",
    owner_client_name: "Pi",
    message: "knowing its id grants no observation or control permission",
  });
  // Every field is always present — omitting one is a wire violation.
  const missing = ajv.compile({
    $schema: "https://json-schema.org/draft/2019-09/schema",
    $ref: "#/$defs/SessionSummary",
    $defs: schema.$defs,
  });
  assert.equal(missing({ session_id: "s1" }), false, "summary without nulls must be rejected");
});

test("error codes are the SCREAMING_SNAKE enum; unknown codes are rejected", () => {
  validate("ErrorCode", "OBSERVATION_TOKEN_REQUIRED");
  validate("ErrorCode", "INVALID_DAEMON_ADMIN_TOKEN");
  const bad = ajv.compile({
    $schema: "https://json-schema.org/draft/2019-09/schema",
    $ref: "#/$defs/ErrorCode",
    $defs: schema.$defs,
  });
  assert.equal(bad("NOT_A_REAL_CODE"), false);
});

test("shutdown params document the admin token field", () => {
  validate("ShutdownParams", { admin_token: "ts-fake-admin-token" });
  // Tokenless is schema-valid so the daemon can answer
  // DAEMON_ADMIN_TOKEN_REQUIRED.
  validate("ShutdownParams", {});
});

test("observe params carry the observation capability token slots", () => {
  validate("ObserveParams", {
    session_id: "s1",
    observation_token: "ts-fake-observation-token",
    include_image: true,
  });
  validate("ObserveParams", {
    session_id: "s1",
    control_token: "ts-fake-control-token",
    target: "screen",
    image_format: "png",
  });
});
