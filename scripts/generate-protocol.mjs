#!/usr/bin/env node
// Protocol v3 single-source-of-truth pipeline:
//
//   Rust wire types ──(cu-protocol-gen)──▶ protocol/computer-use.schema.json
//   schema ──(json-schema-to-typescript)──▶ packages/*/src/generated/protocol.ts
//
// `pnpm generate:protocol` writes all artifacts. `pnpm check:protocol` (this
// script with --check) regenerates everything and fails with a diff when any
// committed artifact has drifted from the Rust source of truth — adapters must
// never hand-edit the wire format.

import { spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import jst from "json-schema-to-typescript";
const { compile } = jst;

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const schemaPath = join(repoRoot, "protocol", "computer-use.schema.json");
const adapterPackages = [
  "packages/sdk-typescript",
  "packages/mcp-server",
  "packages/pi-extension",
];
const check = process.argv.includes("--check");

const header = `/* eslint-disable */
/**
 * GENERATED FILE — do not edit by hand.
 * Source of truth: the Rust wire types (crates/cu-core/src/protocol.rs etc.).
 * Regenerate with \`pnpm generate:protocol\`; \`pnpm check:protocol\` fails on drift.
 */
`;

// 1. Regenerate the JSON Schema from Rust — the single source of truth.
// In --check mode the binary prints the document to stdout so it can be
// compared against the committed file without touching the disk.
const cargo = spawnSync("cargo", ["run", "-q", "-p", "cu-protocol-gen"], {
  cwd: repoRoot,
  encoding: "utf8",
  env: {
    ...process.env,
    PATH: `${process.env.HOME}/.cargo/bin:${process.env.PATH}`,
    ...(check ? { CU_PROTOCOL_GEN_STDOUT: "1" } : {}),
  },
});
if (cargo.status !== 0) {
  process.stderr.write(`${cargo.stdout}\n${cargo.stderr}`);
  process.exit(1);
}

const expectedSchema = check ? `${cargo.stdout.trim()}\n` : readFileSync(schemaPath, "utf8");
const schema = JSON.parse(expectedSchema);
const meta = schema["x-protocol-meta"];

// 2. Generate the TypeScript bindings once, from the schema.
const types = await compile(schema, "ComputerUseProtocol", {
  bannerComment: "",
  unreachableDefinitions: true,
});
const constants = `\nexport const PROTOCOL_VERSION = ${meta.protocol_version};\nexport const MINIMUM_CLIENT_PROTOCOL_VERSION = ${meta.minimum_client_protocol_version};\nexport const MAXIMUM_CLIENT_PROTOCOL_VERSION = ${meta.maximum_client_protocol_version};\nexport const JSONRPC_VERSION = "${meta.jsonrpc_version}";\n`;
const generated = header + types + constants;

// 3. Write (or compare) the artifacts.
const artifacts = [{ path: schemaPath, content: expectedSchema }];
for (const pkg of adapterPackages) {
  artifacts.push({
    path: join(repoRoot, pkg, "src", "generated", "protocol.ts"),
    content: generated,
  });
}

const mismatches = [];
for (const artifact of artifacts) {
  if (check) {
    let existing = null;
    try {
      existing = readFileSync(artifact.path, "utf8");
    } catch {
      existing = null;
    }
    if (existing !== artifact.content) {
      mismatches.push(artifact.path.replace(`${repoRoot}/`, ""));
    }
  } else {
    mkdirSync(dirname(artifact.path), { recursive: true });
    writeFileSync(artifact.path, artifact.content);
  }
}

if (check) {
  if (mismatches.length > 0) {
    console.error(
      `Protocol drift detected — run \`pnpm generate:protocol\` and commit the changes:\n${mismatches
        .map((m) => `  - ${m}`)
        .join("\n")}`,
    );
    process.exit(1);
  }
  console.log("protocol artifacts are up to date");
} else {
  console.log(`wrote ${artifacts.length} protocol artifacts (${adapterPackages.length} TS bindings + schema)`);
}
