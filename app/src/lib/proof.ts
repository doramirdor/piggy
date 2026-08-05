// What the Proof screen says, derived from the headline payload. Kept pure and
// dependency-free so the decisions - which verdict, which arm is short, whether
// there is a blocker the user can click away - are unit-testable without React.
//
// Copy split with the backend, deliberately: `Headline.note` is one honest
// sentence and it stays the source for the Overview hero and `piggy report`,
// which have room for exactly one line. Proof has room for a card with a button,
// so it derives its own title/detail/fix from the STRUCTURE of the headline
// (`onRandomized`, the arm counts, the baseline kind) and falls back to the note
// verbatim for any case it does not recognise. Nothing here invents a reason the
// payload did not already state.

import type { Headline, SaverRow, Waiting } from "../types";
import { commafy } from "./format";

export type ProofTone = "measured" | "estimated" | "waiting";

/** Why an arm is or is not usable. `short` is "keep going"; `unusable` is "this
 *  will never settle on its own", and they must not be drawn the same way: a
 *  hand-pinned ON arm can hold 9,790 sessions and still back nothing, which
 *  rendered as the nonsense "9,790 of 10" when the only distinction was a
 *  count. */
export type ArmState = "ready" | "short" | "unusable";

export interface ProofArm {
  key: "on" | "off";
  label: string;
  /** The honest qualifier: what these sessions actually are. */
  qual: string;
  /** Every session on this arm, including the ones that cannot back the badge. */
  n: number;
  /** The sessions that CAN back it, which is what the sample bar is applied to
   *  and what the rail draws. Equal to `n` everywhere except a pooled ON arm,
   *  where `n` is thousands of hand-set sessions and this is the handful the
   *  scheduler chose. Drawing `n` there filled the rail to the brim under the
   *  word "not randomized", which reads as done. */
  usable: number;
  /** Sessions needed on this arm (MIN_GROUP). */
  target: number;
  state: ArmState;
  ready: boolean;
}

export interface ProofBlocker {
  title: string;
  detail: string;
  /** Savers to hand back to rotation. Empty when the fix is not a button. */
  unpin: string[];
}

/** The "why is this still measuring" panel, in three plain sentences.
 *
 *  Every one of these answers a question a user actually asked out loud: what is
 *  it even measuring, why is the number so low when I have thousands of
 *  sessions, and how long until it finishes. A progress bar answers none of
 *  them - the arms already draw one, and the screen still read as broken. */
export interface ProofWait {
  /** What the comparison is, before any numbers. Always present while the
   *  verdict is unsettled: "what is it even measuring" is the question a user
   *  has whether the hold-up is sample size or a pinned saver. */
  what: string;
  /** How far along, and on which side. Null when sample size is NOT the
   *  hold-up: a count next to "pinned by hand" reads as a countdown, and that
   *  arm does not fill by waiting however long the user leaves it. */
  progress: string | null;
  /** Why the count is where it is. Null when there is nothing to explain. */
  because: string | null;
  /** How much longer, or an honest refusal to guess. Null for the same reason
   *  `progress` is: no ETA on a wait that would never end. */
  eta: string | null;
}

/** The day a timestamp falls on, in the user's locale. The hour a saver set came
 *  together is noise; the day is what they can match to "right, I installed
 *  something on Tuesday". */
function dayOf(iso: string): string | null {
  const d = new Date(iso);
  return Number.isNaN(d.getTime())
    ? null
    : d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

/** The sentence that answers "what is this screen even measuring". Said in every
 *  unsettled state, because it is the question underneath all of them. */
const WHAT =
  "Piggy is comparing sessions that run your savers against sessions it runs with every saver off, measuring tokens per assistant turn on both sides. It needs enough of each before it will put a number on the difference.";

function waitView(w: Waiting): ProofWait {
  const onArm = w.arm === "on";
  const day = w.since ? dayOf(w.since) : null;
  return {
    what: WHAT,
    progress: `${w.have} of ${w.need} ${onArm ? "sessions on your current saver set" : "all-off sessions held back"}`,
    // The ON arm restarting is the single most confusing thing this screen does,
    // and it was invisible: install one saver and a count in the thousands drops
    // to zero, because sessions running a different set are not evidence about
    // the set you run now. Say it, with the date, or the screen reads as stuck.
    because: onArm
      ? day
        ? `Your saver set last changed on ${day}, and the count restarted then. Sessions that ran a different set are still on file, they just are not evidence about the setup you have now.`
        : "The count restarts whenever your saver set changes, because sessions that ran a different set are not evidence about the setup you have now."
      : day
        ? `Piggy has been holding sessions back since ${day}. Roughly one session in ten is a holdout, so this side fills slowly by design.`
        : "Roughly one session in ten is held back all-off, so this side fills slowly by design.",
    eta:
      w.daysLeft == null
        ? "Too early to estimate how long - Piggy needs a couple of sessions to know your pace."
        : w.daysLeft < 1.5
          ? "About a day to go at your recent pace."
          : `About ${Math.ceil(w.daysLeft)} days to go at your recent pace.`,
  };
}

/** The wait for an ON arm whose randomized sessions are pooled with hand-set
 *  ones. `Headline.waiting` is null here by construction - it reads the pooled
 *  `nFullOn`, which cleared the bar thousands of sessions ago - so this is the
 *  only place the real progress can be said. No ETA: the pace Piggy tracks is
 *  for the arm as a whole, and extrapolating it would promise a date the
 *  randomized subset has not earned. */
function pooledWait(h: Headline): ProofWait {
  const rest = h.nFullOn - h.nFullOnRandomized;
  return {
    what: WHAT,
    progress: `${h.nFullOnRandomized} of ${h.minGroup} sessions Piggy chose the setup for`,
    because:
      `Your other ${commafy(rest)} sessions on this saver set ran because you switched it on ` +
      `by hand. They still count toward the estimate, but a setup Piggy did not choose cannot ` +
      `settle the claim however many sessions it holds, so the count above is the one that ends ` +
      `the wait.`,
    eta: null,
  };
}

export interface ProofView {
  tone: ProofTone;
  /** Short verdict for the chip: "Proven" / "Estimated" / "Not proven yet". */
  verdict: string;
  /** The multiplier, when the headline has one to stand behind. */
  multiplier: number | null;
  /** The big line when there is no multiplier: what is missing, in one sentence. */
  claim: string | null;
  /** The line under the claim. */
  sub: string;
  arms: ProofArm[];
  /** Worst first, and often empty. More than one can apply at once: a setup that
   *  is not being rotated AND an estimate withheld for costing more are two
   *  separate facts, and the one-sentence `sub` can only carry the first. */
  blockers: ProofBlocker[];
  /** Null when the claim is settled, or when sample size is not what is holding
   *  it up - in which case `blocker` names the real reason and a "still
   *  gathering" panel would be the wrong story. */
  wait: ProofWait | null;
}

function offArm(h: Headline): ProofArm {
  const n = h.nBaseline;
  // Nothing is pooled into the baseline: `pick_baseline` picks one population
  // and uses it alone, so every session here counts.
  // A contaminated holdout is still evidence - it caps the claim at `estimated`
  // rather than disqualifying the arm - so the OFF side is only ever ready or
  // short. What it actually is gets said in `qual`.
  const ready = n >= h.minGroup;
  const base = {
    key: "off" as const,
    n,
    usable: n,
    target: h.minGroup,
    state: (ready ? "ready" : "short") as ArmState,
    ready,
  };
  switch (h.baselineKind) {
    case "holdout":
      return {
        ...base,
        label: "Savers off",
        qual: h.baselineClean
          ? "holdout sessions Piggy ran with everything off"
          : "holdout sessions, but a saver you pinned kept running",
      };
    case "pre_install":
      return {
        ...base,
        label: "Before Piggy",
        qual: "your own history, observational rather than randomized",
      };
    default:
      return { ...base, label: "Savers off", qual: "no baseline sessions yet" };
  }
}

/** "rtk", "rtk and barber", "rtk, barber and caveman". */
function listOf(names: string[]): string {
  if (names.length <= 1) return names[0] ?? "";
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

function onArm(h: Headline): ProofArm {
  const n = h.nFullOn;
  // The count the gate is actually applied to. `nFullOn` is what the arm HOLDS;
  // once the randomized sessions fall short of the bar the backend pools the
  // hand-set ones in beside them, and only the randomized subset can carry a
  // measured claim.
  const usable = h.onRandomized ? n : h.nFullOnRandomized;
  // Randomization is checked before the count, and that order is the point: a
  // hand-pinned arm does not become usable by growing, so calling it "short"
  // would promise that waiting fixes it.
  //
  // But "not randomized" is two situations wearing one flag, and they want
  // opposite promises. Nothing rotating (no randomized session at all) really is
  // unusable and needs a button. Rotation running with the arm still short is an
  // experiment in progress - the ordinary `short` case - and calling THAT
  // unusable told a user four sessions from the finish line that waiting was
  // pointless, under a rail drawn full from sessions that were not counting.
  const state: ArmState =
    usable === 0 && !h.onRandomized ? "unusable" : usable >= h.minGroup ? "ready" : "short";
  return {
    key: "on",
    label: "Your saver set on",
    // A carried-forward arm is not the plain thing its count suggests, and the
    // qualifier is the only place that can say so before the reader trusts the
    // number: some of these sessions ran a different set, and they are here
    // because the saver that differed was measured to do nothing.
    qual: !h.onRandomized
      ? usable === 0
        ? "pinned on by hand, so these are not randomized"
        : `only ${usable} ran a setup Piggy chose · the other ${commafy(n - usable)} you switched on by hand`
      : h.nCarried > 0
        ? `includes ${h.nCarried} from an earlier setup, where only ${listOf(h.carriedSavers)} differed and measured as no change`
        : "sessions Piggy chose the setup for",
    n,
    usable,
    target: h.minGroup,
    state,
    ready: state === "ready",
  };
}

/** Every blocker the screen can name better than the note can, worst first.
 *
 *  A list rather than one card, because there really can be two at once and the
 *  note has room for exactly one sentence. A setup that is not being rotated AND
 *  whose estimate was withheld for costing more is the common pair: the note
 *  leads with the randomization gap (correctly - it is the root cause), and the
 *  suppression then had nowhere to be said at all, so the screen showed a "no
 *  number yet" that was true for a reason it never gave. */
function blockersFor(h: Headline, savers: SaverRow[], onUsable: number): ProofBlocker[] {
  if (h.label === "measured") return [];
  const out: ProofBlocker[] = [];

  // The root cause when it applies, and the only one with a one-click fix:
  // nothing is being rotated, so no amount of waiting will produce a contrast.
  // Gated on the randomized count, not on `onRandomized` alone - an arm with
  // sessions Piggy chose is rotating, and telling that user to hand a saver back
  // sends them to a Savers tab with nothing pinned in it.
  if (!h.onRandomized && h.nFullOnRandomized === 0) {
    const pinned = savers.filter((s) => s.pinned && s.enabled);
    if (pinned.length > 0) {
      const n = pinned.length;
      out.push({
        title: `${n === 1 ? `${pinned[0].name} is` : `All ${n} running savers are`} pinned by hand.`,
        detail:
          "Piggy respects that and stops rotating them, so it never gets a session with them off to compare against.",
        unpin: pinned.map((s) => s.id),
      });
    } else {
      out.push({
        title: "Your sessions ran with savers set by hand.",
        detail:
          "Piggy only measures setups it chose itself. New sessions will count once it is rotating again.",
        unpin: [],
      });
    }
  }

  // Independent of everything above: the data is in and the number was withheld
  // on purpose. Stacks, because it is a fact about the result rather than about
  // the sample, and it survives the arms filling up.
  if (h.multiplierState === "withheld_cost_more") {
    out.push({
      title: "Your savers came out costing more per turn, not less.",
      detail:
        "Piggy will not publish a figure below 1× off a comparison it did not randomize: heavier recent work is a likelier cause than a real regression. The per-stream rows below show which stream it is, and the number arrives with the sign proved once the ON arm is randomized.",
      unpin: [],
    });
  }

  if (out.length > 0) return out;

  // Both arms are filling normally: that is progress, not a blocker. The rails
  // already show it, so do not put a warning card over an experiment that is
  // simply running. Measured against the USABLE count, since that is the one
  // the gate reads - `nFullOn` clears the bar on an arm that cannot back
  // anything.
  const short = onUsable < h.minGroup || h.nBaseline < h.minGroup;
  if (short) return [];

  // Enough sessions on both sides and still no number, for a reason this module
  // has no structure to name. The backend's sentence is the honest fallback.
  return h.note ? [{ title: "No number yet.", detail: h.note, unpin: [] }] : [];
}

export function proofView(h: Headline | null, savers: SaverRow[]): ProofView | null {
  if (!h) return null;

  const measured = h.label === "measured" && h.value != null;
  const estimated = h.label === "estimated" && h.value != null;
  const arms = [offArm(h), onArm(h)];
  const [off, on] = arms;

  // What is actually missing, said plainly. "measuring…" told the user nothing
  // about whether Piggy was working or stuck, and this is the sentence that
  // replaces it.
  let claim: string | null = null;
  if (!measured && !estimated) {
    if (off.n === 0 && on.n === 0) {
      claim = "Piggy has not seen either side of the comparison yet.";
    } else if (!h.onRandomized && h.nFullOnRandomized > 0) {
      // Both arms hold sessions and the ON one is simply short of randomized
      // ones. "One side, not both" is the wrong story for that: it describes a
      // missing arm, when the arm is there and counting.
      claim = `Piggy is ${h.nFullOnRandomized} of ${h.minGroup} sessions into the side it chose the setup for.`;
    } else if (off.ready !== on.ready) {
      claim = "Piggy has one side of the comparison, not both.";
    } else {
      claim = "Piggy has both sides, but not a number it will stand behind.";
    }
  }

  return {
    tone: measured ? "measured" : estimated ? "estimated" : "waiting",
    verdict: measured ? "Proven" : estimated ? "Estimated" : "Not proven yet",
    multiplier: measured || estimated ? h.value : null,
    claim,
    // The backend names the reason whenever there is one to name, and its
    // wording is asserted by its own tests - do not restate it here. The
    // fallback only fires when it sent none, and it says the policy rather than
    // guessing at a cause.
    sub: measured
      ? `measured against ${h.nBaseline} holdout sessions · the × is price-weighted, so it is an estimate on top of measured streams`
      : (h.note ?? "Piggy shows nothing here rather than a guess."),
    arms,
    blockers: blockersFor(h, savers, on.usable),
    // Present for every unsettled verdict, because "what is it measuring" is
    // always worth answering. The countdown half is not: a hand-pinned ON arm
    // does not fill by waiting, so it gets the explanation with no progress and
    // no ETA, and the blocker card next to it names the fix.
    wait:
      measured || estimated
        ? null
        : h.onRandomized && h.waiting
          ? waitView(h.waiting)
          : // Rotating, and short of randomized sessions rather than of sessions.
            // The payload's own `waiting` cannot see this arm (it reads the
            // pooled count), so this is the one case the module has to build the
            // progress itself rather than pass one through.
            !h.onRandomized && h.nFullOnRandomized > 0
            ? pooledWait(h)
            : { what: WHAT, progress: null, because: null, eta: null },
  };
}
