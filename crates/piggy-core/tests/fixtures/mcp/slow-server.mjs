#!/usr/bin/env node
// Answers `initialize` and then goes quiet forever: the probe's timeout has to
// stop it, kill it, and report a timeout rather than hanging the caller.
//
// The safety timer is a backstop for a probe that fails to kill its child - the
// process would otherwise outlive the test run.

import { createInterface } from "node:readline";

setTimeout(() => process.exit(0), 120_000).unref();

const rl = createInterface({ input: process.stdin });

rl.on("line", (line) => {
  const text = line.trim();
  if (text === "") return;
  let msg;
  try {
    msg = JSON.parse(text);
  } catch {
    return;
  }
  if (msg.method !== "initialize" || msg.id === undefined) return;
  process.stdout.write(
    JSON.stringify({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        protocolVersion: "2025-06-18",
        capabilities: { tools: {} },
        serverInfo: { name: "slow-server", version: "1.0.0" },
      },
    }) + "\n",
  );
  // Everything after this point is silence: no tools/list answer, ever.
});

// Keep the process alive after stdin closes so a probe that forgets to kill it
// leaves evidence (a stray process) instead of exiting quietly.
rl.on("close", () => {});
