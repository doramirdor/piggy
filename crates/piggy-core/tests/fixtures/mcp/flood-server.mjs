#!/usr/bin/env node
// Bursts server-to-client requests and never reads its stdin: the shape that
// used to park the probe in the kernel forever.
//
// The probe answers a server-to-client request with a -32601 refusal so a server
// waiting on a client capability does not deadlock. Every refusal is a *write*,
// and the probe's timeout only guards reads, so a server that asks for enough
// things and then stops reading fills the pipe (64 KiB on macOS, around refusal
// 552) and `write_all` blocks with no deadline - with this process orphaned on
// the other end of it. The probe has to stop replying long before that.
//
// Deliberate details:
//   * REQUESTS is well past the point where the pipe fills, so the fixture
//     proves the ceiling and not the pipe size;
//   * stdin is never read, so nothing the probe writes is ever drained;
//   * no request is ever answered, so the only way out is the ceiling;
//   * the process stays alive after the burst, so a probe that fails to kill its
//     child leaves a stray process for the test to find.

const REQUESTS = 700;

let out = "";
for (let i = 1; i <= REQUESTS; i += 1) {
  out += `${JSON.stringify({
    jsonrpc: "2.0",
    id: i,
    method: "roots/list",
    params: {},
  })}\n`;
}
process.stdout.write(out);

// Stay alive and silent. The backstop is for a probe that forgets to kill its
// child: the process would otherwise outlive the test run.
setInterval(() => {}, 1_000);
setTimeout(() => process.exit(0), 120_000);
