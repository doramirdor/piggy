import { useEffect, useState } from "react";
import { useStore } from "../store";
import { AdviceSheet } from "../screens/AdviceSheet";
import { figureLine, hiddenCount, openItems, topOpen, totalLine } from "../lib/advice";

/**
 * What to do next, at the top of Spend.
 *
 * Spend answers "where did my tokens go". Without this it is an autopsy: a
 * beautiful breakdown with no verb. The three biggest things the engine can
 * actually change sit above the table, each as a plain claim with the figure it
 * rests on, and everything behind Review is reversible.
 *
 * Advice is pull, not push. No badge, no dot, no notification, no auto-refresh
 * nag: this asks once when the screen mounts and then holds still.
 */
export function AdviceSection() {
  const advice = useStore((s) => s.advice);
  const loadAdvice = useStore((s) => s.loadAdvice);
  const [sheet, setSheet] = useState<{ focusId?: string } | null>(null);

  useEffect(() => {
    void loadAdvice();
  }, [loadAdvice]);

  // Three states, never conflated. "Nothing to suggest" is a claim, and a list
  // that has not landed yet must not make it.
  if (advice === null) {
    return (
      <>
        <div className="sect">What to do next</div>
        <div className="attr advice-attr">
          {[0, 1].map((i) => (
            <div className="arow skel" key={i} aria-hidden>
              <div className="aname">
                <div className="sk sk-line" />
              </div>
              <div className="sk sk-line sk-short" />
            </div>
          ))}
        </div>
        <div className="foot-note">Checking what there is to act on…</div>
      </>
    );
  }

  const top = topOpen(advice.items);
  if (top.length === 0) {
    return (
      <>
        <div className="sect">What to do next</div>
        <div className="foot-note">
          Nothing to act on right now. Every add-on is in use, your CLAUDE.md files are clean, and
          no saver's own measurements argue for a different setting.
        </div>
      </>
    );
  }

  const open = openItems(advice.items).length;
  const more = hiddenCount(advice.items);
  return (
    <>
      <div className="sect">
        What to do next
        <span className="sect-sub">
          {open} suggestion{open === 1 ? "" : "s"}, ranked by estimated tokens a month
        </span>
      </div>
      <div className="attr advice-attr">
        {top.map((a) => (
          <div className="arow" key={a.id}>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div className="aname">{a.title}</div>
              {/* No `useCountUp` here. The tick is reserved for the one hero
                  figure per screen and Spend's hero already owns it; a second
                  ticking number on a screen that refreshes on a 400ms debounce
                  is a permanent flicker. */}
              <div className="an" style={{ whiteSpace: "normal" }}>
                {figureLine(a)}
              </div>
            </div>
            <button className="btn" onClick={() => setSheet({ focusId: a.id })}>
              Review
            </button>
          </div>
        ))}
        <div className="arow">
          <div className="an" style={{ flex: 1, minWidth: 0, whiteSpace: "normal" }}>
            {totalLine(advice.items)}
            {more > 0 && ` ${more} more not shown.`}
          </div>
          <button className="btn" onClick={() => setSheet({})}>
            Review all
          </button>
        </div>
      </div>

      {sheet && <AdviceSheet focusId={sheet.focusId} onClose={() => setSheet(null)} />}
    </>
  );
}
