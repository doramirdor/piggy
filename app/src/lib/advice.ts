// Every decision the advice surface makes, as pure functions.
//
// The components below this are dumb renderers on purpose: `vite.config.ts` runs
// vitest in a node environment with no DOM, so a component test would not run at
// all, and a rule that only lives inside JSX is a rule nothing guards. Same
// convention as `lib/badge.ts`.
//
// One rule governs the whole file. **The app never maps a basis string to a
// different word, and never re-derives an evidence number.** The engine computed
// both together; this side prints them side by side and gets out of the way.

import { formatTokens } from "./format";
import type { AdviceFailure, AdviceItem } from "../types";

/** How confident a figure's label is allowed to look. */
export type BasisTone = "observed" | "measured" | "estimated" | "waiting";

/** Rows the Spend section shows before it defers to the sheet. */
export const SECTION_MAX = 3;

/** What a `stale` row says for itself when the backend sent no reason. */
export const STALE_NOTE =
  "Piggy re-checked and the numbers behind this moved, so the plan no longer describes your " +
  "setup. It comes back on the next scan if it still applies.";

/**
 * The chip's text: the engine's own word, uppercased. No table, no mapping, no
 * exceptions.
 *
 * `basis::ESTIMATED_AB` is the literal string "estimated (observational)". Any
 * shortening of that to MEASURED because it came out of the A/B machinery is
 * exactly the defect this surface exists to avoid.
 */
export function basisLabel(basis: string): string {
  return basis.toUpperCase();
}

/**
 * The only thing mapped: the colour.
 *
 * An unrecognised basis colours as an estimate, never as a measurement. A later
 * milestone adding a seventh constant must degrade to hedged without anyone
 * touching this file.
 */
export function basisTone(basis: string): BasisTone {
  switch (basis) {
    case "observed":
      return "observed";
    case "measured":
    case "measured manifest":
      return "measured";
    case "not enough data yet":
      return "waiting";
    default:
      return "estimated";
  }
}

/** The CSS class carrying that tone, on the existing `.lins-tag` primitive. */
export function basisClass(basis: string): string {
  switch (basisTone(basis)) {
    case "observed":
      return "obs";
    case "measured":
      return "msr";
    case "waiting":
      return "wait";
    default:
      return "est";
  }
}

function plural(n: number, one: string, many: string): string {
  return n === 1 ? one : many;
}

/**
 * The one-line figure under a suggestion's title.
 *
 * A trim's `estTokensMonth` is what the file COSTS, not what applying gives
 * back: how much a rewrite removes is not known until it is drafted. The burden
 * form therefore never says "saves". This is the same class of defect as a
 * basis label, not a wording preference.
 */
export function figureLine(a: AdviceItem): string {
  const tokens = `~${formatTokens(a.estTokensMonth)} tokens a month`;
  return a.figureKind === "burden"
    ? `costs ${tokens} · estimated`
    : `${tokens} · estimated`;
}

/** Open suggestions only. Applied, dismissed and stale are not things to act on. */
export function openItems(items: AdviceItem[]): AdviceItem[] {
  return items.filter((a) => a.status === "open");
}

/**
 * The first few open suggestions, in the engine's order.
 *
 * Never re-sorted here. The engine ranks by estimated tokens a month with ties
 * broken on id so the same facts produce the same list, and a second sort on
 * this side would quietly become a second ranking nobody documented.
 */
export function topOpen(items: AdviceItem[], max = SECTION_MAX): AdviceItem[] {
  return openItems(items).slice(0, max);
}

/** Whether the checkbox appears and the id may be sent to apply. */
export function canApply(a: AdviceItem): boolean {
  return a.applyable && a.status === "open";
}

/** Why this one cannot be applied, in one sentence, or null when it can. */
export function blockedNote(a: AdviceItem): string | null {
  if (canApply(a)) return null;
  if (a.blockedReason) return a.blockedReason;
  if (a.status === "stale") return STALE_NOTE;
  return null;
}

/**
 * The ids Apply actually sends.
 *
 * Re-intersected with the live list at click time, not at selection time: an
 * item that went stale while the sheet was open must not be sent, and a
 * selection is a memory of what the list looked like a moment ago.
 */
export function applyIds(selected: Iterable<string>, items: AdviceItem[]): string[] {
  const live = new Map(items.map((a) => [a.id, a]));
  const out: string[] = [];
  for (const id of selected) {
    const item = live.get(id);
    if (item && canApply(item)) out.push(id);
  }
  return out;
}

/**
 * Groups in the order they first appear in the ranking, so the biggest single
 * opportunity still leads the sheet. Matches `piggy advise`.
 */
export function byGroup(items: AdviceItem[]): { group: string; items: AdviceItem[] }[] {
  const out: { group: string; items: AdviceItem[] }[] = [];
  for (const a of items) {
    const hit = out.find((g) => g.group === a.group);
    if (hit) hit.items.push(a);
    else out.push({ group: a.group, items: [a] });
  }
  return out;
}

/**
 * What a set of open suggestions is worth a month: the savings.
 *
 * A sum of the engine's own per-item figures, split on the engine's own
 * `figureKind`. Not a re-derivation of an evidence value, which is the thing
 * this side never does; it is the same arithmetic `advice::total_savings` does,
 * over a slice the backend was not asked about. `agreesWithReport` below is the
 * guard that the two answers match.
 */
export function savingsTokens(items: AdviceItem[]): number {
  return openItems(items)
    .filter((a) => a.figureKind === "saves")
    .reduce((n, a) => n + a.estTokensMonth, 0);
}

/** The other half, kept apart: what the oversized files cost as they stand. */
export function burdenTokens(items: AdviceItem[]): number {
  return openItems(items)
    .filter((a) => a.figureKind === "burden")
    .reduce((n, a) => n + a.estTokensMonth, 0);
}

/**
 * The Spend section's footer, and the sheet's subtitle above it.
 *
 * Savings and burden are two clauses, never one total. Summed, an oversized
 * global file loaded two hundred times a month dwarfs every real saving on the
 * list, and the reader would be told that applying everything gives back a
 * number nothing measured.
 *
 * Takes the items it describes rather than the whole report, because the sheet
 * can be opened filtered to one group: a total over the whole list, printed
 * over two of its rows, is a wrong number in the most believable place.
 */
export function totalLine(items: AdviceItem[]): string {
  const open = openItems(items);
  const saving = open.filter((a) => a.figureKind === "saves").length;
  const burden = open.filter((a) => a.figureKind === "burden").length;
  const parts: string[] = [];
  if (saving > 0) {
    parts.push(
      `~${formatTokens(savingsTokens(open))} tokens a month across ` +
        `${saving} ${plural(saving, "suggestion", "suggestions")}.`,
    );
  }
  if (burden > 0) {
    parts.push(
      `Plus ${burden} oversized ${plural(burden, "file", "files")} costing ` +
        `~${formatTokens(burdenTokens(open))} tokens a month as ` +
        `${plural(burden, "it stands", "they stand")}.`,
    );
  }
  if (parts.length === 0) {
    return `${open.length} ${plural(open.length, "suggestion", "suggestions")} to review.`;
  }
  return parts.join(" ");
}

/** The sheet's subtitle: the same accounting, plus the promise. */
export function sheetSummary(items: AdviceItem[]): string {
  const open = openItems(items);
  if (open.length === 0) return "Nothing to act on right now.";
  return `${totalLine(open)} Ranked by estimated saving. Nothing changes until you apply it.`;
}

/** How many open suggestions the Spend section did not have room for. */
export function hiddenCount(items: AdviceItem[], max = SECTION_MAX): number {
  return Math.max(0, openItems(items).length - max);
}

/** The banner heading, pluralised for the verb that failed. */
export function failureTitle(n: number, verb: "apply" | "undo"): string {
  if (verb === "apply") {
    return n === 1
      ? "1 suggestion couldn't be applied"
      : `${n} suggestions couldn't be applied`;
  }
  return n === 1 ? "1 item couldn't be put back" : `${n} items couldn't be put back`;
}

/**
 * What a failure is called in the banner.
 *
 * An apply failure's id is a sixteen-character hash, which tells a reader
 * nothing. Undo reports the file, server or saver by name already, so that falls
 * through unchanged.
 */
export function failureLabel(f: AdviceFailure, items: AdviceItem[]): string {
  return items.find((a) => a.id === f.id)?.title ?? f.id;
}

/**
 * The size of an edit, in lines.
 *
 * Read off the diff the engine computed, not off the evidence rows: "Lines
 * removed" is a count of what the transform dropped and this is a count of what
 * the view is showing, and they are allowed to differ when the view is
 * truncated. Only the disclosure's contents quote it, because the counts are
 * not known until the diff is fetched.
 */
export function diffCounts(removed: number, added: number): string {
  return (
    `${removed} ${plural(removed, "line", "lines")} out, ` +
    `${added} ${plural(added, "line", "lines")} in`
  );
}
