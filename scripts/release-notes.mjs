// Generate the GitHub pre-release notes for the given tag.
//
//   node scripts/release-notes.mjs <tag> [--out <path>]
//
// Reads .github/release-notes.template.md, substitutes the tag / runtime
// version / protocol version, and appends the changelog section for that
// version. The template's honest-status sections (signing, notarization, Pi
// host, OpenCode model-driven, same-UID threat model) are maintained by
// hand in the template — this script only substitutes facts that must never
// drift (version numbers), so a release can never claim a version that is
// not the one being released. Prints the notes to stdout, or writes them
// with --out.
import { readFileSync, writeFileSync } from "node:fs";
import { resolve, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const tag = args[0];
const outIdx = args.indexOf("--out");
const outPath = outIdx >= 0 ? args[outIdx + 1] : null;

if (!tag || !/^v/.test(tag)) {
  console.error("usage: node scripts/release-notes.mjs <tag v*> [--out <path>]");
  process.exit(2);
}
const version = tag.replace(/^v/, "");

// Protocol version: from the schema itself (single source of truth), never
// hardcoded in the template.
const schema = JSON.parse(readFileSync(join(REPO, "protocol/computer-use.schema.json"), "utf8"));
const protocolVersion = schema["x-protocol-meta"]?.protocol_version;
if (!Number.isInteger(protocolVersion)) {
  console.error("release-notes: no x-protocol-meta.protocol_version in protocol/computer-use.schema.json");
  process.exit(1);
}

// Changelog section for this version: the first "## <version>" block up to
// the next "## ". Missing section → warn (notes still generated, the
// template's status sections remain the source of truth).
const changelog = readFileSync(join(REPO, "CHANGELOG.md"), "utf8");
const marker = `## ${version}`;
const sectionStart = changelog.indexOf(marker);
let section = "";
if (sectionStart >= 0) {
  const next = changelog.indexOf("\n## ", sectionStart + marker.length);
  section = changelog.slice(sectionStart, next >= 0 ? next : undefined).trim();
} else {
  console.error(`release-notes: warning — CHANGELOG.md has no "## ${version}" section`);
}

const template = readFileSync(join(REPO, ".github/release-notes.template.md"), "utf8")
  .replaceAll("{{VERSION}}", version)
  .replaceAll("{{TAG}}", tag)
  .replaceAll("{{PROTOCOL_VERSION}}", String(protocolVersion));

const notes = `${template}\n\n## Changelog (${version})\n\n${section || "_No changelog section for this version yet._"}\n`;

if (outPath) {
  writeFileSync(resolve(REPO, outPath), notes);
  console.log(`release-notes: wrote ${resolve(REPO, outPath)}`);
} else {
  process.stdout.write(notes);
}
