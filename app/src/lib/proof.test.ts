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
  nFullOnRandomized: 0,
  baselineClean: false,
  multiplierState: "no_data",
  minGroup: 10,
  streams: [],
  turns: null,
  waiting: null,
  nCarried: 0,
  carriedSavers: [],
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
    expect(v.blockers).toEqual([]);
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
    expect(v.blockers).toHaveLength(1);
    expect(v.blockers[0].title).toContain("All 2 running savers are pinned");
    expect(v.blockers[0].unpin).toEqual(["ponytail", "caveman"]);
  });

  it("names a single pinned saver rather than pluralizing over one", () => {
    const v = proofView(headline({ nBaseline: 151 }), [saver()])!;
    expect(v.blockers[0].title).toBe("Ponytail is pinned by hand.");
  });

  it("offers no un-pin button when nothing pinned is actually running", () => {
    // The ON arm still rests on old hand-set sessions, but there is no longer a
    // pinned saver to hand back, so a button here would do nothing.
    const v = proofView(headline({ nBaseline: 151 }), [saver({ pinned: false })])!;
    expect(v.blockers).toHaveLength(1);
    expect(v.blockers[0].unpin).toEqual([]);
  });

  it("does not cry blocker while both arms are simply filling up", () => {
    const v = proofView(
      headline({ onRandomized: true, baselineKind: "holdout", nFullOn: 4, nBaseline: 6 }),
      [saver({ pinned: false })],
    )!;
    expect(v.blockers).toEqual([]);
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
    expect(v.blockers[0].detail).toBe("your recent sessions cost more per turn than your history");
    expect(v.blockers[0].unpin).toEqual([]);
  });

  it("drops the gathering panel when both arms are full and the blocker owns the story", () => {
    // Two cards, two stories: the blocker says why there is no number, and a
    // "still measuring" panel beside it would promise that waiting fixes what
    // waiting cannot fix.
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
    expect(v.wait).toBeNull();
    expect(v.blockers.length).toBeGreaterThan(0);
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

// The state a real store spends most of its life in, and the one the screen got
// most wrong: rotation IS running, the ON arm holds thousands of sessions, and
// only a handful of them are the randomized ones the badge depends on. Every
// assertion here is a sentence the old screen got backwards.
describe("proofView · a pooled ON arm", () => {
  const pooled = (over = {}) =>
    headline({
      baselineKind: "holdout",
      baselineClean: true,
      nBaseline: 155,
      nHoldout: 155,
      nFullOn: 9792,
      nFullOnRandomized: 5,
      onRandomized: false,
      ...over,
    });

  it("counts the arm in randomized sessions, not in the ones it merely holds", () => {
    const on = proofView(pooled(), [saver({ pinned: false })])!.arms[1];
    expect(on.n).toBe(9792);
    expect(on.usable).toBe(5);
    // `short`, not `unusable`: five more scheduler-chosen sessions finish this,
    // and "unusable" promised the opposite.
    expect(on.state).toBe("short");
    expect(on.qual).toContain("only 5 ran a setup Piggy chose");
    expect(on.qual).toContain("9,787");
  });

  it("does not send a rotating user to un-pin savers that are not pinned", () => {
    // The whole bug, in one assertion. `onRandomized: false` used to be enough to
    // print "Piggy never switches them off · hand one back in the Savers tab" at
    // someone whose savers were being switched off all day.
    expect(proofView(pooled(), [saver()])!.blockers).toEqual([]);
  });

  it("says how far along the randomized side is, and promises no date for it", () => {
    const v = proofView(pooled(), [])!;
    expect(v.claim).toBe("Piggy is 5 of 10 sessions into the side it chose the setup for.");
    expect(v.wait?.progress).toBe("5 of 10 sessions Piggy chose the setup for");
    expect(v.wait?.because).toContain("9,787");
    // Piggy tracks a pace for the arm as a whole, which says nothing about how
    // fast the randomized subset fills. No number beats a made-up one.
    expect(v.wait?.eta).toBeNull();
  });

  it("keeps the pin blocker for an arm with no randomized session at all", () => {
    const v = proofView(pooled({ nFullOnRandomized: 0 }), [saver()])!;
    expect(v.arms[1].state).toBe("unusable");
    expect(v.blockers[0].unpin).toEqual(["ponytail"]);
  });
});

describe("proofView · a withheld estimate", () => {
  it("names the suppression as its own blocker instead of leaving it unsaid", () => {
    // Enough sessions on both sides, both arms randomized, and still no number:
    // the estimate came out below 1× and was withheld on purpose. Structure alone
    // cannot see that, so before `multiplierState` the screen fell through to
    // "No number yet" with the backend's sentence, or to nothing at all.
    const v = proofView(
      headline({
        onRandomized: true,
        baselineKind: "holdout",
        nFullOn: 30,
        nBaseline: 40,
        multiplierState: "withheld_cost_more",
      }),
      [],
    )!;
    expect(v.blockers).toHaveLength(1);
    expect(v.blockers[0].title).toContain("costing more per turn");
  });

  it("stacks with the pin blocker, worst first, since both are true at once", () => {
    const v = proofView(
      headline({ nBaseline: 155, baselineKind: "holdout", multiplierState: "withheld_cost_more" }),
      [saver()],
    )!;
    expect(v.blockers.map((b) => b.title)).toEqual([
      "Ponytail is pinned by hand.",
      "Your savers came out costing more per turn, not less.",
    ]);
  });
});

describe("proofView · the wait", () => {
  const waiting = (over = {}) => ({
    arm: "on" as const,
    have: 4,
    need: 10,
    since: "2026-08-04T13:33:52.412Z",
    daysLeft: 6,
    ...over,
  });

  it("explains a restarted ON arm with the date it restarted", () => {
    // The whole point. A user with 10,000 sessions sees "4 of 10" and concludes
    // Piggy is broken; the missing fact is that installing a saver restarts the
    // count, and when.
    const v = proofView(
      headline({ nFullOn: 4, onRandomized: true, waiting: waiting() }),
      [],
    )!;
    expect(v.wait?.progress).toBe("4 of 10 sessions on your current saver set");
    expect(v.wait?.because).toContain("saver set last changed");
    expect(v.wait?.eta).toContain("6 days");
  });

  it("refuses to guess an ETA it does not have", () => {
    const v = proofView(
      headline({ nFullOn: 1, onRandomized: true, waiting: waiting({ have: 1, daysLeft: null }) }),
      [],
    )!;
    expect(v.wait?.eta).toContain("Too early");
  });

  it("explains itself but promises no ETA when waiting is not the blocker", () => {
    // A hand-pinned ON arm does not fill by waiting, so a countdown there would
    // be a countdown to nothing. The explanation still belongs on screen; the
    // blocker card owns the fix.
    const pinned = proofView(
      headline({ nFullOn: 4, onRandomized: false, waiting: waiting() }),
      [],
    )!;
    expect(pinned.wait?.what).toContain("comparing sessions");
    expect(pinned.wait?.progress).toBeNull();
    expect(pinned.wait?.eta).toBeNull();
    // And a settled claim has nothing left to wait for.
    const done = proofView(
      headline({ label: "measured", value: 1.4, nFullOn: 20, nBaseline: 20, onRandomized: true }),
      [],
    )!;
    expect(done.wait).toBeNull();
  });
});

describe("proofView · carried-forward sessions", () => {
  it("says on the arm itself that some sessions came from an older setup", () => {
    // The count alone would be a lie by omission: 18 sessions, but 12 of them
    // ran a set the user has since changed. They are here because the saver that
    // differed was measured to do nothing, and the arm has to say so.
    const v = proofView(
      headline({
        value: 1.5,
        label: "estimated",
        baselineKind: "holdout",
        baselineClean: true,
        onRandomized: true,
        nFullOn: 18,
        nBaseline: 40,
        nCarried: 12,
        carriedSavers: ["barber"],
      }),
      [],
    )!;
    expect(v.tone).toBe("estimated");
    expect(v.arms[1].n).toBe(18);
    expect(v.arms[1].qual).toContain("12 from an earlier setup");
    expect(v.arms[1].qual).toContain("barber");
    expect(v.arms[1].qual).toContain("no change");
  });

  it("lists several carried savers readably", () => {
    const v = proofView(
      headline({
        value: 1.5,
        label: "estimated",
        onRandomized: true,
        nFullOn: 18,
        nBaseline: 40,
        nCarried: 12,
        carriedSavers: ["barber", "caveman", "nadir-route"],
      }),
      [],
    )!;
    expect(v.arms[1].qual).toContain("barber, caveman and nadir-route");
  });

  it("leaves the ordinary arm wording alone when nothing was carried", () => {
    const v = proofView(headline({ onRandomized: true, nFullOn: 12 }), [])!;
    expect(v.arms[1].qual).toBe("sessions Piggy chose the setup for");
  });
});
