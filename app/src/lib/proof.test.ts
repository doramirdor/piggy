import { describe, it, expect } from "vitest";
import { proofView } from "./proof";
import type { Headline, SaverRow } from "../types";

const HEADLINE: Headline = {
  value: null,
  label: "not_enough_data",
  nHoldout: 0,
  note: null,
  nFullOn: 0,
  nBaseline: 0,
  baselineKind: "none",
  onRandomized: false,
  baselineClean: false,
  minGroup: 10,
  streams: [],
};

function headline(over: Partial<Headline> = {}): Headline {
  return { ...HEADLINE, ...over };
}

function saver(over: Partial<SaverRow> = {}): SaverRow {
  return {
    id: "ponytail",
    name: "Ponytail",
    plainLabel: null,
    description: "",
    installType: "hook",
    status: "curated_v1",
    defaultOn: false,
    installed: true,
    enabled: true,
    pinned: true,
    installable: true,
    behaviorChanging: false,
    warning: null,
    risk: null,
    claimedSavings: null,
    license: "MIT",
    licenseNote: null,
    ordering: 0,
    badge: { kind: "measuring", delta: null, n: 0, nOn: 0, nOff: 0 },
    configurable: false,
    launchCommand: null,
    ...over,
  };
}

describe("proofView", () => {
  it("returns null with no headline, so the screen shows its loading state", () => {
    expect(proofView(null, [])).toBeNull();
  });

  it("calls a holdout-measured headline proven and keeps the × labelled an estimate", () => {
    const v = proofView(
      headline({
        value: 1.6,
        label: "measured",
        baselineKind: "holdout",
        baselineClean: true,
        onRandomized: true,
        nFullOn: 38,
        nBaseline: 151,
        nHoldout: 151,
      }),
      [],
    )!;
    expect(v.tone).toBe("measured");
    expect(v.verdict).toBe("Proven");
    expect(v.multiplier).toBe(1.6);
    expect(v.claim).toBeNull();
    expect(v.sub).toContain("151 holdout sessions");
    // The multiplier is price-weighted even when the streams behind it are
    // randomized, and this screen is the one place that must never blur that.
    expect(v.sub).toContain("price-weighted");
    // Nothing is blocking a headline that already landed.
    expect(v.blocker).toBeNull();
  });

  it("hedges an observational headline rather than calling it proven", () => {
    const v = proofView(
      headline({
        value: 1.3,
        label: "estimated",
        baselineKind: "pre_install",
        onRandomized: true,
        nFullOn: 20,
        nBaseline: 900,
        note: "estimated vs your history · holdout measurement in progress",
      }),
      [],
    )!;
    expect(v.tone).toBe("estimated");
    expect(v.verdict).toBe("Estimated");
    expect(v.multiplier).toBe(1.3);
    expect(v.arms[0].label).toBe("Before Piggy");
    expect(v.arms[0].qual).toContain("observational");
    // The backend's own sentence, not one this module invented.
    expect(v.sub).toBe("estimated vs your history · holdout measurement in progress");
  });

  it("names one full arm and one empty arm instead of a bare 'measuring'", () => {
    const v = proofView(
      headline({
        baselineKind: "holdout",
        baselineClean: true,
        nBaseline: 151,
        nHoldout: 151,
        nFullOn: 0,
        onRandomized: false,
      }),
      [saver()],
    )!;
    expect(v.tone).toBe("waiting");
    expect(v.verdict).toBe("Not proven yet");
    expect(v.claim).toBe("Piggy has one side of the comparison, not both.");
    const [off, on] = v.arms;
    expect(off.n).toBe(151);
    expect(off.ready).toBe(true);
    expect(on.n).toBe(0);
    expect(on.ready).toBe(false);
  });

  it("blocks on pinned savers, names them, and offers them for un-pinning", () => {
    const savers = [saver(), saver({ id: "caveman", name: "Caveman" })];
    const v = proofView(headline({ nBaseline: 151, baselineKind: "holdout" }), savers)!;
    expect(v.blocker).not.toBeNull();
    expect(v.blocker!.title).toContain("All 2 running savers are pinned");
    expect(v.blocker!.unpin).toEqual(["ponytail", "caveman"]);
  });

  it("names a single pinned saver rather than pluralizing over one", () => {
    const v = proofView(headline({ nBaseline: 151 }), [saver()])!;
    expect(v.blocker!.title).toBe("Ponytail is pinned by hand.");
  });

  it("offers no un-pin button when nothing pinned is actually running", () => {
    // The ON arm still rests on old hand-set sessions, but there is no longer a
    // pinned saver to hand back, so a button here would do nothing.
    const v = proofView(headline({ nBaseline: 151 }), [saver({ pinned: false })])!;
    expect(v.blocker).not.toBeNull();
    expect(v.blocker!.unpin).toEqual([]);
  });

  it("does not cry blocker while both arms are simply filling up", () => {
    const v = proofView(
      headline({ onRandomized: true, baselineKind: "holdout", nFullOn: 4, nBaseline: 6 }),
      [saver({ pinned: false })],
    )!;
    expect(v.blocker).toBeNull();
    expect(v.claim).toBe("Piggy has both sides, but not a number it will stand behind.");
  });

  it("surfaces the backend's reason when both arms are full and there is still no number", () => {
    const v = proofView(
      headline({
        onRandomized: true,
        baselineKind: "holdout",
        nFullOn: 30,
        nBaseline: 40,
        note: "your recent sessions cost more per turn than your history",
      }),
      [],
    )!;
    expect(v.blocker!.detail).toBe("your recent sessions cost more per turn than your history");
    expect(v.blocker!.unpin).toEqual([]);
  });

  it("says nothing has been seen when neither arm has a single session", () => {
    const v = proofView(headline({ onRandomized: true }), [])!;
    expect(v.claim).toBe("Piggy has not seen either side of the comparison yet.");
  });

  it("marks a contaminated holdout as one, so the baseline is not oversold", () => {
    const v = proofView(
      headline({ baselineKind: "holdout", baselineClean: false, nBaseline: 151 }),
      [],
    )!;
    expect(v.arms[0].qual).toContain("a saver you pinned kept running");
  });

  it("never calls a hand-pinned ON arm ready, however many sessions it has", () => {
    // The count alone clears the bar; randomization does not. Treating this arm
    // as ready would paint a green rail over the exact thing that is blocking.
    const v = proofView(headline({ nFullOn: 400, onRandomized: false }), [saver()])!;
    expect(v.arms[1].n).toBe(400);
    expect(v.arms[1].ready).toBe(false);
    expect(v.arms[1].qual).toContain("not randomized");
  });

  it("calls a plentiful-but-unrandomized arm unusable, never merely short", () => {
    // Real numbers from a live database: 9,790 sessions on the ON arm, none of
    // them randomized. `short` renders as "N of 10", which read as the nonsense
    // "9,790 of 10" and implied that waiting would fix it. It will not.
    const v = proofView(headline({ nFullOn: 9790, nBaseline: 151 }), [saver()])!;
    expect(v.arms[1].state).toBe("unusable");
    // And a genuinely short arm stays short, so the "N of 10" progress copy
    // survives where it is still true.
    const filling = proofView(headline({ nFullOn: 4, onRandomized: true }), [])!;
    expect(filling.arms[1].state).toBe("short");
  });
});
