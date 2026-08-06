import { Fragment, useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useStore } from "../store";
import { AdviceDiff } from "../components/AdviceDiff";
import {
  applyIds,
  basisClass,
  basisLabel,
  blockedNote,
  byGroup,
  canApply,
  failureLabel,
  failureTitle,
  figureLine,
  openItems,
  sheetSummary,
} from "../lib/advice";
import type { AdviceFailure, AdviceItem } from "../types";

/** The engine's own word for how a figure was arrived at, printed verbatim.
 *  Only the colour is mapped, and a basis nobody recognises colours as an
 *  estimate rather than as a measurement. */
function Basis({ basis }: { basis: string }) {
  return <span className={`lins-tag ${basisClass(basis)}`}>{basisLabel(basis)}</span>;
}

/** The date half of an RFC3339 stamp: a row has no room for a timestamp. */
function day(ts: string | null): string {
  return ts?.split("T")[0] ?? "earlier";
}

/** One failure block. Never collapsed into a count: an apply that did three of
 *  four things has to say which one it did not, and why. */
function Failures({
  failures,
  verb,
  items,
  reassurance,
  onDismiss,
}: {
  failures: AdviceFailure[];
  verb: "apply" | "undo";
  items: AdviceItem[];
  reassurance: string;
  onDismiss: () => void;
}) {
  return (
    <div className="banner" role="alert" style={{ marginTop: 12, marginBottom: 0 }}>
      <div style={{ flex: 1, minWidth: 0 }}>
        <div className="btitle">{failureTitle(failures.length, verb)}</div>
        {failures.map((f, i) => (
          <div className="bbody" key={`${f.id}-${i}`} style={{ whiteSpace: "normal" }}>
            <b>{failureLabel(f, items)}</b> · {f.reason}
          </div>
        ))}
        <div className="bbody" style={{ whiteSpace: "normal", color: "var(--text-2)" }}>
          {reassurance}
        </div>
      </div>
      <button className="bclose" onClick={onDismiss} aria-label="Dismiss">
        ×
      </button>
    </div>
  );
}

/**
 * The sheet where a suggestion becomes a decision.
 *
 * Every claim on Spend and on Savers leads here, and here is where the evidence
 * behind it is laid out in full: what was counted, what it came to, and how each
 * figure was arrived at. Content edits show the exact lines that would change
 * before anyone agrees to them. Nothing is applied without a click, everything
 * applied can be put back with one, and an item that could not be done is named
 * with its reason rather than folded into a count.
 */
export function AdviceSheet({
  onClose,
  focusId,
  group,
}: {
  onClose: () => void;
  focusId?: string;
  group?: string;
}) {
  const advice = useStore((s) => s.advice);
  const loadAdvice = useStore((s) => s.loadAdvice);
  const applyAdvice = useStore((s) => s.applyAdvice);
  const undoAdvice = useStore((s) => s.undoAdvice);
  const dismissAdvice = useStore((s) => s.dismissAdvice);
  const showError = useStore((s) => s.showError);

  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [undoing, setUndoing] = useState<string | null>(null);
  const [applyFailures, setApplyFailures] = useState<AdviceFailure[]>([]);
  const [undoFailures, setUndoFailures] = useState<AdviceFailure[]>([]);
  const [warnings, setWarnings] = useState<string[]>([]);
  const focusRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    void loadAdvice();
  }, [loadAdvice]);

  // Escape closes it. The app has no dialog role or key handling anywhere else
  // today; this adds both here rather than leaving the one modal that writes to
  // disk as the one you cannot back out of with the keyboard.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  useEffect(() => {
    focusRef.current?.scrollIntoView({ block: "nearest" });
  }, [advice, focusId]);

  const all = advice?.items ?? [];
  // Both lists take the same filter. An Applied section showing CLAUDE.md edits
  // under an "Add-ons" heading would offer an Undo for something the sheet
  // never claimed to be about.
  const inGroup = (a: AdviceItem) => !group || a.group === group;
  const items = all.filter(inGroup);
  const applied = (advice?.applied ?? []).filter(inGroup);

  const toggle = (id: string) => {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setSelected(next);
  };

  // Re-intersected with the live list at click time: an item that went stale
  // while the sheet was open must not be sent.
  const sendable = applyIds(selected, items);

  const apply = async () => {
    setBusy(true);
    try {
      const res = await applyAdvice(sendable);
      setApplyFailures(res.failures);
      setWarnings(res.warnings);
      const next = new Set(selected);
      for (const id of res.applied) next.delete(id);
      setSelected(next);
    } catch (e) {
      showError(e);
    } finally {
      setBusy(false);
    }
  };

  const undo = async (id: string) => {
    setUndoing(id);
    try {
      const res = await undoAdvice(id);
      setUndoFailures(res.failures);
    } catch (e) {
      showError(e);
    } finally {
      setUndoing(null);
    }
  };

  const dismiss = async (id: string) => {
    setBusy(true);
    try {
      await dismissAdvice(id);
    } catch (e) {
      showError(e);
    } finally {
      setBusy(false);
    }
  };

  // Through a portal, because the page turn leaves its mark. `.page-turn > *`
  // animates `transform` with `fill-mode: both`, so every section keeps an
  // identity matrix after the animation ends - and an ancestor with any
  // transform value other than the `none` keyword becomes the containing block
  // for `position: fixed`. Rendered in place, this sheet's full-screen backdrop
  // covered one column of one screen and centred itself in the middle of a
  // scrolled page, with the Apply button below the fold.
  return createPortal(
    <div className="sheet-backdrop" onClick={onClose}>
      <div
        className="sheet advice-sheet"
        role="dialog"
        aria-modal="true"
        aria-labelledby="advice-title"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="stitle" id="advice-title">
          {group ?? "What to do next"}
        </div>
        <div className="ssub">
          {advice === null ? "Checking what there is to act on…" : sheetSummary(items)}
        </div>

        <div className="advice-body">
          {byGroup(openItems(items)).map((g) => (
            <div key={g.group}>
              {!group && <div className="sect">{g.group}</div>}
              {g.items.map((a) => (
                <div
                  className="acard"
                  key={a.id}
                  ref={a.id === focusId ? focusRef : undefined}
                >
                  <div className="ahead">
                    {canApply(a) && (
                      <label className="share-check">
                        <input
                          type="checkbox"
                          checked={selected.has(a.id)}
                          disabled={busy}
                          onChange={() => toggle(a.id)}
                          aria-label={`Select ${a.title}`}
                        />
                      </label>
                    )}
                    <div className="atitle">{a.title}</div>
                  </div>
                  <div className="an" style={{ whiteSpace: "normal" }}>
                    {figureLine(a)}
                  </div>

                  {/* Label, value, basis - the basis sits in the same row as
                      the number it describes, never in a legend somewhere else. */}
                  <div className="aev">
                    {a.evidence.map((e, i) => (
                      <Fragment key={i}>
                        <span className="aev-label">{e.label}</span>
                        <span className="aev-value">{e.value}</span>
                        <Basis basis={e.basis} />
                      </Fragment>
                    ))}
                  </div>

                  {a.hasDiff && (
                    <details className="adet">
                      {/* Lazy: the diff is fetched when this opens, not when the
                          sheet does. The line counts live inside, where they are
                          the engine's own answer rather than a guess made to fill
                          a summary. */}
                      <summary className="dsum">View the changes</summary>
                      <AdviceDiff id={a.id} />
                    </details>
                  )}

                  {blockedNote(a) && <div className="ablocked">{blockedNote(a)}</div>}

                  {/* Dismiss is a thing to say about a suggestion, not about a
                      change already on disk: an applied row's only route back is
                      its restore handle, and "not for me" would drop it. */}
                  {a.status === "open" && (
                    <div className="acard-actions">
                      <button className="btn" disabled={busy} onClick={() => dismiss(a.id)}>
                        Not for me
                      </button>
                    </div>
                  )}
                </div>
              ))}
            </div>
          ))}

          {advice !== null && openItems(items).length === 0 && (
            <div className="foot-note">
              Nothing to act on here right now. Piggy re-checks every time you open this.
            </div>
          )}

          {applied.length > 0 && (
            <>
              <div className="sect">Applied</div>
              <div className="attr">
                {applied.map((a) => (
                  <div className="arow" key={a.id}>
                    <div style={{ flex: 1, minWidth: 0 }}>
                      <div className="aname">{a.title}</div>
                      <div className="an" style={{ whiteSpace: "normal" }}>
                        Applied {day(a.appliedAt)} · one click puts it back
                      </div>
                    </div>
                    <button
                      className="btn"
                      disabled={undoing !== null}
                      onClick={() => undo(a.id)}
                    >
                      {undoing === a.id ? "Putting it back…" : "Undo"}
                    </button>
                  </div>
                ))}
              </div>
            </>
          )}

          {applyFailures.length > 0 && (
            <Failures
              failures={applyFailures}
              verb="apply"
              items={all}
              reassurance="Nothing was half-done: each one either applied or it did not, and the ones that did are listed under Applied."
              onDismiss={() => setApplyFailures([])}
            />
          )}

          {undoFailures.length > 0 && (
            <Failures
              failures={undoFailures}
              verb="undo"
              items={all}
              reassurance="Nothing was lost: the backups stay on disk and you can retry Undo once the file is writable."
              onDismiss={() => setUndoFailures([])}
            />
          )}

          {/* The engine's own words, verbatim. A warning is something that
              happened, not something Piggy chose to say. */}
          {warnings.map((w, i) => (
            <div className="foot-note" key={i}>
              {w}
            </div>
          ))}

          <div className="foot-note">
            Every figure says how it was arrived at. "Observed" was counted in your session
            database, "measured manifest" is a real byte count of a server's tool schemas,
            "measured" is a randomized A/B result, and "estimated" is arithmetic over one of those.
          </div>
        </div>

        <div className="sactions">
          <button className="btn" onClick={onClose}>
            Done
          </button>
          <button
            className="btn primary"
            disabled={busy || sendable.length === 0}
            onClick={apply}
          >
            {busy
              ? "Applying…"
              : sendable.length === 0
                ? "Apply"
                : `Apply ${sendable.length} selected`}
          </button>
        </div>
      </div>
    </div>,
    document.body,
  );
}
