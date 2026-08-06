import { describe, it, expect } from "vitest";
import { probeRow } from "./probe";
import { basisTone } from "./advice";
import type { ProbeServer } from "../types";

const server = (over: Partial<ProbeServer> = {}): ProbeServer => ({
  key: "github",
  scope: "", // store::SCOPE_USER
  scopeLabel: "Every project",
  transport: "stdio",
  measurement: "never",
  toolCount: null,
  schemaBytes: null,
  schemaTokens: null,
  tokenizer: null,
  tokensEstimated: false,
  measuredAt: null,
  error: null,
  probeable: true,
  ...over,
});

const measured = (over: Partial<ProbeServer> = {}): ProbeServer =>
  server({
    measurement: "measured",
    toolCount: 21,
    schemaBytes: 43_190,
    schemaTokens: 12_340,
    tokenizer: "est-bytes/3.5",
    tokensEstimated: true,
    measuredAt: "2026-08-01",
    ...over,
  });

const fig = (s: ProbeServer, label: string) =>
  probeRow(s).figures.find((f) => f.label === label);

describe("a measured row", () => {
  // The bytes are a real count of what the server sent. Dividing them by 3.5 is
  // not a tokenization, and one label over both printed every probed row as an
  // exact count it never was.
  it("calls the bytes measured and the bytes-over-3.5 token count estimated", () => {
    const s = measured();
    expect(fig(s, "schema bytes")?.basis).toBe("measured manifest");
    expect(fig(s, "tools")?.basis).toBe("measured manifest");
    expect(fig(s, "tokens")?.basis).toBe("estimated");
    // And the tone follows the word, not the row's status.
    expect(basisTone(fig(s, "tokens")!.basis)).toBe("estimated");
    expect(basisTone(fig(s, "schema bytes")!.basis)).toBe("measured");
  });

  it("promotes the token count to measured manifest under a real tokenizer", () => {
    const s = measured({ tokenizer: "qwen3-4b", tokensEstimated: false });
    expect(fig(s, "tokens")?.basis).toBe("measured manifest");
  });

  it("shows the date it was measured and offers to measure again", () => {
    const row = probeRow(measured());
    expect(row.note).toBe("Measured 2026-08-01.");
    expect(row.action).toBe("Measure again");
    expect(row.figures).toHaveLength(3);
  });

  it("keeps the separators on the figures and the tilde on the estimate", () => {
    const s = measured();
    expect(fig(s, "schema bytes")?.value).toBe("43,190");
    expect(fig(s, "tokens")?.value).toBe("~12,340");
  });
});

describe("a row with nothing to show", () => {
  // The stored numbers describe a command that is not what runs today. There is
  // no label under which printing them would be true.
  it("shows no figures at all when the config changed under the measurement", () => {
    const row = probeRow(measured({ measurement: "stale" }));
    expect(row.figures).toEqual([]);
    expect(row.note).toContain("command changed");
    expect(row.action).toBe("Measure again");
  });

  it("prints the probe's own reason for a failure and offers a retry", () => {
    const row = probeRow(
      server({
        measurement: "failed",
        measuredAt: "2026-08-01",
        error: "timed out after 10s with no answer; the server was stopped",
      }),
    );
    expect(row.note).toBe(
      "Piggy started it on 2026-08-01 and timed out after 10s with no answer; the server was stopped.",
    );
    expect(row.action).toBe("Try again");
    expect(row.figures).toEqual([]);
  });

  it("says sweep is estimating for a server nobody has measured", () => {
    const row = probeRow(server());
    expect(row.note).toContain("Sweep is using a size estimate");
    expect(row.action).toBe("Measure");
    expect(row.figures).toEqual([]);
  });

  // There is nothing Piggy will run for an http server, so there is no button
  // to run it with.
  it("gives an http server no button", () => {
    const row = probeRow(server({ measurement: "deferred", transport: "remote", probeable: false }));
    expect(row.action).toBeNull();
    expect(row.note).toContain("Sweep keeps its estimate");
  });
});
