#!/usr/bin/env node
// Writes plain text where MCP requires newline-delimited JSON: the probe must
// call it a parse error immediately, not wait out its timeout.
//
// It also echoes `PIGGY_FAKE_TOKEN` on both streams when that env var is set -
// the redaction test's forcing function. A server printing its own credential
// is exactly how a secret would otherwise end up in an error string and, from
// there, in the database.

import { createInterface } from "node:readline";

const token = process.env.PIGGY_FAKE_TOKEN;

// The token line goes FIRST when there is one, so the very first thing the probe
// reads (and quotes back in its parse error) is the credential.
if (token) process.stdout.write(`config loaded: token=${token}\n`);
process.stdout.write("garbage-server v1.0 starting up\n");
process.stdout.write("<html><body>not json at all</body></html>\n");
process.stderr.write(`garbage-server: authenticating with ${token ?? "no token"}\n`);

setTimeout(() => process.exit(0), 120_000).unref();

// Stay up: whether the probe classifies this as garbage must not depend on
// winning a race against the process exiting.
const rl = createInterface({ input: process.stdin });
rl.on("line", () => {});
rl.on("close", () => {});
