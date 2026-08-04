#!/usr/bin/env node
// `cu-opencode` — companion CLI for using the computer-use runtime from
// OpenCode. OpenCode talks to the runtime through the MCP server; this CLI
// only sets that wiring up and keeps it healthy:
//
//   cu-opencode setup            write ~/.config/opencode/opencode.json with the MCP entry
//   cu-opencode setup --print    print the config fragment instead of writing
//   cu-opencode status           daemon health + active session
//   cu-opencode session cleanup  stop the active session
//   cu-opencode doctor           environment check
//   cu-opencode help
import { connect, defaultSocketPath } from "@computer-use/sdk";

import {
  cleanupSession,
  defaultOpenCodeConfigPath,
  doctor,
  doctorText,
  generateOpenCodeConfig,
  statusText,
  writeOpenCodeConfig,
} from "./index.js";

function fail(message: string): never {
  console.error(`cu-opencode: ${message}`);
  console.error("try `cu-opencode help`");
  process.exit(1);
}

function printHelp(): void {
  console.log(`cu-opencode — computer-use companion for OpenCode

OpenCode connects to the computer-use runtime via its MCP server
(@computer-use/mcp-server, binary "computer-use-mcp"); this CLI generates
that configuration and inspects the daemon.

Usage:
  cu-opencode setup [--path <config.json>] [--print]
      Add the computer-use MCP server to your OpenCode config
      (default: ${defaultOpenCodeConfigPath()}).
      --print prints the fragment to stdout instead of writing.
  cu-opencode status [--socket <path>]
      Show daemon health and the active session.
  cu-opencode session cleanup [--socket <path>]
      Stop the active session (idempotent).
  cu-opencode doctor [--socket <path>]
      Check binaries, socket, and daemon health.
  cu-opencode help

Environment: COMPUTER_USE_SOCKET overrides the daemon socket path
(default: ${defaultSocketPath()}).
`);
}

async function main(argv: string[]): Promise<void> {
  const [cmd, ...rest] = argv;
  if (!cmd || cmd === "help" || cmd === "--help" || cmd === "-h") {
    printHelp();
    return;
  }

  const socketFlag = rest.indexOf("--socket");
  const socketPath = socketFlag >= 0
    ? rest[socketFlag + 1] ?? fail("--socket requires a path")
    : process.env.COMPUTER_USE_SOCKET ?? defaultSocketPath();

  switch (cmd) {
    case "setup": {
      const pathFlag = rest.indexOf("--path");
      const path = pathFlag >= 0
        ? rest[pathFlag + 1] ?? fail("--path requires a path")
        : defaultOpenCodeConfigPath();
      if (rest.includes("--print")) {
        console.log(JSON.stringify(generateOpenCodeConfig(), null, 2));
        return;
      }
      const result = await writeOpenCodeConfig(path);
      console.log(
        `${result.existed ? "updated" : "wrote"} ${result.path}${result.merged ? " (existing computer-use entry replaced)" : ""}`,
      );
      if (result.backup) console.log(`backup: ${result.backup}`);
      if (!result.changed) console.log("config already up to date (no changes, no backup created)");
      console.log("restart opencode (or run its MCP reload) to pick up the MCP server.");
      return;
    }
    case "status": {
      const client = await connect({ socketPath }).catch((err) => fail(`cannot connect to daemon at ${socketPath}: ${err.message}`));
      try {
        console.log(await statusText(client));
      } finally {
        client.close();
      }
      return;
    }
    case "session": {
      if (rest[0] !== "cleanup") fail("expected `session cleanup`");
      const client = await connect({ socketPath }).catch((err) => fail(`cannot connect to daemon at ${socketPath}: ${err.message}`));
      try {
        const result = await cleanupSession(client);
        console.log(result.message);
      } finally {
        client.close();
      }
      return;
    }
    case "doctor": {
      const report = await doctor({ socketPath });
      console.log(doctorText(report));
      if (report.errors.length > 0) process.exitCode = 1;
      return;
    }
    default:
      fail(`unknown command "${cmd}"`);
  }
}

// ESM bin entry: `node dist/cli.js`. Keep `main` callable from tests.
export { main };

if (process.argv[1]?.endsWith("dist/cli.js")) {
  void main(process.argv.slice(2));
}
