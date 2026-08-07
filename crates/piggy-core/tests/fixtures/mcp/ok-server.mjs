#!/usr/bin/env node
// A minimal MCP stdio server for the probe tests: enough of the protocol to be
// measured, and nothing else.
//
// Deliberate details the probe has to get right:
//   * it answers `initialize` with a DIFFERENT protocol revision than the probe
//     asks for (the probe must accept whatever the server speaks);
//   * it emits a notification before the initialize response (no id: skipped);
//   * it sends the client a `roots/list` REQUEST and will not answer
//     `tools/list` until that request has been replied to, so a probe that
//     ignores server-to-client requests hangs instead of passing;
//   * it pages `tools/list`: two tools, a `nextCursor`, then the third.
//
// Deterministic by construction: no clock, no randomness, no env, no network.

import { createInterface } from "node:readline";

const TOOLS = [
  {
    name: "add",
    description: "Add two numbers and return the sum.",
    inputSchema: {
      type: "object",
      properties: {
        a: { type: "number", description: "First addend." },
        b: { type: "number", description: "Second addend." },
      },
      required: ["a", "b"],
    },
  },
  {
    name: "search",
    description: "Search the fixture corpus for a phrase.",
    inputSchema: {
      type: "object",
      properties: {
        query: { type: "string", description: "The phrase to look for." },
        limit: { type: "integer", description: "How many hits to return." },
      },
      required: ["query"],
    },
  },
  {
    name: "echo",
    description: "Return the text it was given, unchanged.",
    inputSchema: {
      type: "object",
      properties: {
        text: { type: "string", description: "Text to echo back." },
      },
      required: ["text"],
    },
  },
];

const ROOTS_ID = "srv-roots-1";
let rootsAnswered = false;
const pendingLists = [];

function send(msg) {
  process.stdout.write(JSON.stringify(msg) + "\n");
}

function answerList(msg) {
  const cursor = msg.params && msg.params.cursor;
  if (!cursor) {
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: { tools: TOOLS.slice(0, 2), nextCursor: "page-2" },
    });
  } else if (cursor === "page-2") {
    send({ jsonrpc: "2.0", id: msg.id, result: { tools: TOOLS.slice(2) } });
  } else {
    send({
      jsonrpc: "2.0",
      id: msg.id,
      error: { code: -32602, message: "unknown cursor" },
    });
  }
}

function drain() {
  if (!rootsAnswered) return;
  while (pendingLists.length > 0) answerList(pendingLists.shift());
}

const rl = createInterface({ input: process.stdin });

rl.on("line", (line) => {
  const text = line.trim();
  if (text === "") return;
  let msg;
  try {
    msg = JSON.parse(text);
  } catch {
    return; // the probe only ever sends JSON; ignore anything else
  }

  // Our own outstanding request, answered (result or error, both count).
  if (msg.id === ROOTS_ID) {
    rootsAnswered = true;
    drain();
    return;
  }
  if (msg.id === undefined || msg.id === null) return; // a notification

  if (msg.method === "initialize") {
    send({
      jsonrpc: "2.0",
      method: "notifications/message",
      params: { level: "info", data: "ok-server starting" },
    });
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        protocolVersion: "2024-11-05",
        capabilities: { tools: {} },
        serverInfo: { name: "ok-server", version: "1.0.0" },
      },
    });
    send({ jsonrpc: "2.0", id: ROOTS_ID, method: "roots/list" });
  } else if (msg.method === "tools/list") {
    pendingLists.push(msg);
    drain();
  } else {
    send({
      jsonrpc: "2.0",
      id: msg.id,
      error: { code: -32601, message: "method not found" },
    });
  }
});

rl.on("close", () => process.exit(0));
