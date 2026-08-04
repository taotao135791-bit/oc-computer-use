#!/usr/bin/env node
// Bin entry: `computer-use-mcp`. A separate launcher (instead of the
// index.ts entry check) so the bin keeps working when npm links it — a
// symlinked bin gets a different process.argv[1] than the real module URL,
// which would make a URL-based entry check silently no-op.
import { main } from "./index.js";

main().catch((err) => {
  console.error(`computer-use MCP server failed: ${err.message}`);
  process.exit(1);
});
