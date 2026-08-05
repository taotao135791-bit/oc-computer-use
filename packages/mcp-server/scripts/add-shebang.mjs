// Post-build hardening for the `computer-use-mcp` bin.
//
// The real bin entry is `dist/bin.js` (declared in package.json `bin`) —
// NOT `dist/index.js` (the library entry). This script:
//
//   1. fails the build if the bin file is missing (never silently exit 0);
//   2. prepends `#!/usr/bin/env node` if — and only if — it is not already
//      there (idempotent: tsc emits a shebang from source *or* the script
//      adds one; either way the result is a single shebang);
//   3. chmods the file to 0755, so the tarball ships an executable bin
//      regardless of what the filesystem umask produced.
//
// npm installs the tarball's file modes verbatim, so a bin that is not
// executable here is a `spawn computer-use-mcp EACCES` for every consumer —
// this script is the only place that fix can live.
import { chmodSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const dist = join(dirname(fileURLToPath(import.meta.url)), "..", "dist");
const binPath = join(dist, "bin.js");

if (!existsSync(binPath)) {
  throw new Error(
    `bin entry not found: ${binPath} (build it with tsc before add-shebang)`,
  );
}

const src = readFileSync(binPath, "utf8");
if (!src.startsWith("#!/usr/bin/env node")) {
  writeFileSync(binPath, "#!/usr/bin/env node\n" + src);
}

// 0755: owner rwx + group/other rx — executable by npm's .bin shim and by
// direct invocation. Anything narrower is EACCES for other users on shared
// installs; the tarball preserves this mode verbatim.
chmodSync(binPath, 0o755);
