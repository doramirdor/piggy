import { describe, it, expect } from "vitest";
import {
  applyIds,
  basisClass,
  basisLabel,
  basisTone,
  blockedNote,
  burdenTokens,
  byGroup,
  canApply,
  diffCounts,
  failureLabel,
  failureTitle,
  figureLine,
  hiddenCount,
  savingsTokens,
  sheetSummary,
  topOpen,
  totalLine,
} from "./advice";
import type { AdviceItem, AdviceReport } from "../types";

/** One suggestion, with only the fields the function under test reads spelled
 *  out. */
const item = (over: Partial<AdviceItem> = {}): AdviceItem =>
  ({
    id: "a1",
    kind: "server-disable",
    group: "Add-ons",
    target: "github",
    title: "Turn off the github server",
    evidence: [],
    estTokensMonth: 12_000,
    figureKind: "saves",
    riskTier: 1,
    status: "open",
    hasDiff: false,
    applyable: true,
    blockedReason: null,
    appliedAt: null,
    ...over,
  }) as AdviceItem;

const report = (items: AdviceItem[], over: Partial<AdviceReport> = {}): AdviceReport => ({
  items,
  applied: [],
  estTokensMonth: items
    .filter((a) => a.status === "open" && a.figureKind === "saves")
    .reduce((n, a) => n + a.estTokensMonth, 0),
  estTokensMonthBurden: items
    .filter((a) => a.status === "open" && a.figureKind === "burden")
    .reduce((n, a) => n + a.estTokensMonth, 0),
  generatedAt: "2026-08-06T10:00:00Z",
  advisorRanked: false,
  ...over,
});

describe("the basis label", () => {
  // THE regression this whole surface exists to prevent. A seventh basis
  // constant landing in a later milestone must degrade to hedged, not to
  // confident, without anybody touching this file.
  it("renders an unrecognised basis hedged, never measured", () => {
    expect(basisTone("some future basis nobody has written yet")).toBe("estimated");
    expect(basisClass("some future basis nobody has written yet")).toBe("est");
  });

  // `basis::ESTIMATED_AB` is an A/B figure on an observational baseline. It came
  // out of the measurement machinery and it is still not a measurement.
  it("prints the engine's word verbatim, uppercased", () => {
    expect(basisLabel("estimated (observational)")).toBe("ESTIMATED (OBSERVATIONAL)");
    expect(basisLabel("estimated (observational)")).not.toBe("MEASURED");
    expect(basisTone("estimated (observational)")).toBe("estimated");
  });

  it("gives each of the engine's six bases its own tone", () => {
    const table: [string, string][] = [
      ["observed", "observed"],
      ["estimated", "estimated"],
      ["measured manifest", "measured"],
      ["measured", "measured"],
      ["estimated (observational)", "estimated"],
      ["not enough data yet", "waiting"],
    ];
    for (const [basis, tone] of table) {
      expect(basisTone(basis), basis).toBe(tone);
      expect(basisLabel(basis)).toBe(basis.toUpperCase());
    }
  });

  it("keeps a measured manifest and a plain estimate apart", () => {
    expect(basisTone("measured manifest")).not.toBe(basisTone("estimated"));
  });
});

describe("the Spend section", () => {
  it("shows open items only", () => {
    const items = [
      item({ id: "open" }),
      item({ id: "applied", status: "applied" }),
      item({ id: "dismissed", status: "dismissed" }),
      item({ id: "stale", status: "stale" }),
    ];
    expect(topOpen(items).map((a) => a.id)).toEqual(["open"]);
  });

  // The engine ranks with ties broken on id so the same facts give the same
  // list. A second sort here would become a second ranking nobody documented.
  it("keeps the engine's order and never re-sorts", () => {
    const items = [
      item({ id: "small", estTokensMonth: 10 }),
      item({ id: "big", estTokensMonth: 900_000 }),
      item({ id: "middle", estTokensMonth: 500 }),
    ];
    expect(topOpen(items).map((a) => a.id)).toEqual(["small", "big", "middle"]);
  });

  it("shows at most three, and says how many it left out", () => {
    const items = [1, 2, 3, 4, 5].map((n) => item({ id: `a${n}` }));
    expect(topOpen(items)).toHaveLength(3);
    expect(hiddenCount(items)).toBe(2);
    expect(hiddenCount(items.slice(0, 2))).toBe(0);
  });
});

describe("figureLine", () => {
  // A trim's figure is the file's monthly BURDEN. "saves ~140k" over it claims
  // a rewrite gives all of that back, which nothing has measured.
  it("says a trim costs it, never that applying saves it", () => {
    const line = figureLine(item({ figureKind: "burden", estTokensMonth: 140_000 }));
    expect(line).toContain("costs");
    expect(line).not.toContain("saves");
    expect(line).toContain("~140k");
  });

  it("says estimated on every figure line, whichever way it reads", () => {
    expect(figureLine(item({ figureKind: "saves" }))).toContain("estimated");
    expect(figureLine(item({ figureKind: "burden" }))).toContain("estimated");
  });
});

describe("applyIds", () => {
  it("sends only the ids that can be applied", () => {
    const items = [item({ id: "ok" }), item({ id: "blocked", applyable: false })];
    expect(applyIds(["ok", "blocked"], items)).toEqual(["ok"]);
  });

  // The list is regenerated by every apply and undo, so a selection made a
  // moment ago can name something the engine no longer offers.
  it("drops an id that has left the list", () => {
    expect(applyIds(["ok", "vanished"], [item({ id: "ok" })])).toEqual(["ok"]);
  });

  it("drops an id that went stale while the sheet was open", () => {
    const items = [item({ id: "a", status: "stale", applyable: false })];
    expect(applyIds(["a"], items)).toEqual([]);
  });
});

describe("the totals", () => {
  it("counts one suggestion in the singular", () => {
    expect(totalLine([item({ estTokensMonth: 12_000 })])).toBe(
      "~12k tokens a month across 1 suggestion.",
    );
  });

  // A burden added to a saving is a claim Piggy has not measured, and it is the
  // biggest number on the list, so it never joins the total.
  it("keeps a burden out of the savings total and in its own clause", () => {
    const items = [
      item({ id: "s", estTokensMonth: 12_000 }),
      item({ id: "b", figureKind: "burden", estTokensMonth: 140_000 }),
    ];
    const line = totalLine(items);
    expect(line).toContain("~12k tokens a month across 1 suggestion.");
    expect(line).toContain("Plus 1 oversized file costing ~140k tokens a month as it stands.");
    expect(line).not.toContain("152k");
    expect(savingsTokens(items)).toBe(12_000);
  });

  // The sheet can be opened filtered to one group, so it totals the rows it is
  // showing. A whole-list figure printed over two of its rows is a wrong number
  // in the most believable place on the screen.
  it("totals only the rows it is describing", () => {
    const items = [
      item({ id: "a", group: "Add-ons", estTokensMonth: 12_000 }),
      item({ id: "b", group: "CLAUDE.md", estTokensMonth: 500_000 }),
    ];
    const addons = items.filter((a) => a.group === "Add-ons");
    expect(totalLine(addons)).toBe("~12k tokens a month across 1 suggestion.");
  });

  // The one place this side's arithmetic is compared with the engine's. If they
  // ever disagree, the app is quoting a total the backend did not compute - and
  // an applied row, which the engine leaves out of both totals, is the case that
  // would drift first.
  it("splits savings from burden the way the engine's own totals do", () => {
    const r = report([
      item({ id: "s", estTokensMonth: 12_000 }),
      item({ id: "b", figureKind: "burden", estTokensMonth: 140_000 }),
      item({ id: "gone", status: "applied", estTokensMonth: 999 }),
    ]);
    expect(savingsTokens(r.items)).toBe(r.estTokensMonth);
    expect(burdenTokens(r.items)).toBe(r.estTokensMonthBurden);
  });

  it("says nothing to act on when the list is empty", () => {
    expect(sheetSummary([])).toBe("Nothing to act on right now.");
  });

  it("promises nothing changes until you apply it", () => {
    expect(sheetSummary([item()])).toContain("Nothing changes until you apply it");
  });
});

describe("failures", () => {
  it("pluralises for both verbs", () => {
    expect(failureTitle(1, "apply")).toBe("1 suggestion couldn't be applied");
    expect(failureTitle(2, "apply")).toBe("2 suggestions couldn't be applied");
    expect(failureTitle(1, "undo")).toBe("1 item couldn't be put back");
    expect(failureTitle(3, "undo")).toBe("3 items couldn't be put back");
  });

  // An apply failure's id is a sixteen-character hash, which tells the reader
  // nothing about what did not happen.
  it("names the suggestion, not its hash", () => {
    const items = [item({ id: "deadbeefdeadbeef", title: "Turn off the github server" })];
    expect(failureLabel({ id: "deadbeefdeadbeef", reason: "no" }, items)).toBe(
      "Turn off the github server",
    );
    expect(failureLabel({ id: "gone", reason: "no" }, items)).toBe("gone");
  });
});

describe("grouping and blocking", () => {
  it("puts groups in the order they first appear in the ranking", () => {
    const items = [
      item({ id: "1", group: "CLAUDE.md" }),
      item({ id: "2", group: "Add-ons" }),
      item({ id: "3", group: "CLAUDE.md" }),
      item({ id: "4", group: "Savers" }),
    ];
    const groups = byGroup(items);
    expect(groups.map((g) => g.group)).toEqual(["CLAUDE.md", "Add-ons", "Savers"]);
    expect(groups[0].items.map((a) => a.id)).toEqual(["1", "3"]);
  });

  it("makes a stale item explain itself, and never applyable", () => {
    const stale = item({ status: "stale", applyable: false, blockedReason: null });
    expect(canApply(stale)).toBe(false);
    expect(blockedNote(stale)).toContain("no longer describes your setup");
  });

  it("prefers the engine's own reason when there is one", () => {
    const blocked = item({
      applyable: false,
      blockedReason: "Turn on the local advisor in Settings for a drafted rewrite.",
    });
    expect(blockedNote(blocked)).toBe(
      "Turn on the local advisor in Settings for a drafted rewrite.",
    );
  });

  it("says nothing about an item that is fine", () => {
    expect(blockedNote(item())).toBeNull();
  });
});

describe("diffCounts", () => {
  it("sizes the edit in lines, singular and plural", () => {
    expect(diffCounts(12, 0)).toBe("12 lines out, 0 lines in");
    expect(diffCounts(1, 1)).toBe("1 line out, 1 line in");
  });
});
