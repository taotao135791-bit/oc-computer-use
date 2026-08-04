// Prepends the node shebang to dist/index.js so the `computer-use-mcp` bin
// works when npm-link installed (a bin needs a shebang to be executable).
import { readFileSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const dist = join(dirname(fileURLToPath(import.meta.url)), "..", "dist", "index.js");
const src = readFileSync(dist, "utf8");
if (!src.startsWith("#!/usr/bin/env node")) {
  writeFileSync(dist, "#!/usr/bin/env node\n" + src);
}
