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

import type { Headline, SaverRow } from "../types";

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
  n: number;
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
  blocker: ProofBlocker | null;
}

function offArm(h: Headline): ProofArm {
  const n = h.nBaseline;
  // A contaminated holdout is still evidence - it caps the claim at `estimated`
  // rather than disqualifying the arm - so the OFF side is only ever ready or
  // short. What it actually is gets said in `qual`.
  const ready = n >= h.minGroup;
  const base = {
    key: "off" as const,
    n,
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

function onArm(h: Headline): ProofArm {
  const n = h.nFullOn;
  // Randomization is checked before the count, and that order is the point: a
  // hand-pinned arm does not become usable by growing, so calling it "short"
  // would promise that waiting fixes it.
  const state: ArmState = !h.onRandomized ? "unusable" : n >= h.minGroup ? "ready" : "short";
  return {
    key: "on",
    label: "Your saver set on",
    qual: h.onRandomized
      ? "sessions Piggy chose the setup for"
      : "pinned on by hand, so these are not randomized",
    n,
    target: h.minGroup,
    state,
    ready: state === "ready",
  };
}

/** The blocker, when there is one the screen can name better than the note can. */
function blockerFor(h: Headline, savers: SaverRow[]): ProofBlocker | null {
  if (h.label === "measured") return null;

  // The root cause when it applies, and the only one with a one-click fix: the
  // ON arm is resting on hand-pinned sessions, so Piggy is not rotating anything
  // and no amount of waiting will produce a contrast.
  if (!h.onRandomized) {
    const pinned = savers.filter((s) => s.pinned && s.enabled);
    if (pinned.length > 0) {
      const n = pinned.length;
      return {
        title: `${n === 1 ? `${pinned[0].name} is` : `All ${n} running savers are`} pinned by hand.`,
        detail:
          "Piggy respects that and stops rotating them, so it never gets a session with them off to compare against.",
        unpin: pinned.map((s) => s.id),
      };
    }
    return {
      title: "Your sessions ran with savers set by hand.",
      detail:
        "Piggy only measures setups it chose itself. New sessions will count once it is rotating again.",
      unpin: [],
    };
  }

  // Both arms are filling normally: that is progress, not a blocker. The rails
  // already show it, so do not put a warning card over an experiment that is
  // simply running.
  const short = h.nFullOn < h.minGroup || h.nBaseline < h.minGroup;
  if (short) return null;

  // Enough sessions on both sides and still no number. The backend names why
  // (cost-more suppression, or no comparable spend) better than structure can.
  return h.note ? { title: "No number yet.", detail: h.note, unpin: [] } : null;
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
    blocker: blockerFor(h, savers),
  };
}
