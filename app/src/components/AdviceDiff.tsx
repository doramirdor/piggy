import { useEffect, useState } from "react";
import { api } from "../ipc";
import { commafy } from "../lib/format";
import { basisClass, basisLabel, diffCounts } from "../lib/advice";
import type { AdviceDiff as AdviceDiffData } from "../types";

/** One basis chip, the engine's word printed verbatim and only its colour
 *  mapped. Local to this file: the sheet has its own, on its own row shape. */
function Basis({ basis }: { basis: string }) {
  return <span className={`lins-tag ${basisClass(basis)}`}>{basisLabel(basis)}</span>;
}

/**
 * The exact edit a CLAUDE.md suggestion would make, before anybody agrees to it.
 *
 * A content edit is the one thing Piggy does that changes prose the user wrote,
 * so it is the one thing that is never applied unseen. The rows come from the
 * engine already computed: the app renders a diff, it does not compute one, and
 * the line numbers here are the line numbers in the file.
 *
 * Fetched on mount, which is lazy because it mounts inside a closed disclosure.
 */
export function AdviceDiff({ id }: { id: string }) {
  const [diff, setDiff] = useState<AdviceDiffData | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    api
      .adviceDiff(id)
      .then((d) => {
        if (live) setDiff(d);
      })
      .catch((e: unknown) => {
        // Inline, not the global banner: opening a disclosure that cannot answer
        // is a local disappointment, and the rest of the sheet still works.
        const detail = (e as { detail?: string })?.detail;
        if (live) setError(detail ?? "Piggy could not read that file.");
      });
    return () => {
      live = false;
    };
  }, [id]);

  if (error) return <div className="an" style={{ whiteSpace: "normal" }}>{error}</div>;
  if (!diff) return <div className="an">Reading the file…</div>;

  return (
    <>
      <div className="an" style={{ whiteSpace: "normal", marginBottom: 5 }}>
        {diffCounts(diff.removed, diff.added)} · {diff.displayPath}
        <br />
        {commafy(diff.beforeBytes)} → {commafy(diff.afterBytes)} bytes{" "}
        <Basis basis="observed" /> · ~{commafy(diff.beforeEstTokens)} → ~
        {commafy(diff.afterEstTokens)} tokens <Basis basis="estimated" />
      </div>

      <div
        className="dwrap"
        role="group"
        aria-label={`Proposed changes to ${diff.displayPath}`}
      >
        {diff.hunks.map((h, i) => (
          <div className="dhunk" key={i}>
            <div className="dhead">{h.header}</div>
            {h.lines.map((l, j) => (
              <div className={`dline ${l.op}`} key={j}>
                <span className="dno">{l.oldNo ?? ""}</span>
                <span className="dno">{l.newNo ?? ""}</span>
                {/* A real character, not only a wash. The palette makes amber
                    and red the same rust on purpose, so meaning has to survive
                    greyscale. */}
                <span className="dmark">
                  {l.op === "add" ? "+" : l.op === "del" ? "-" : " "}
                </span>
                <span className="dtext">{l.text}</span>
              </div>
            ))}
          </div>
        ))}
      </div>

      {diff.truncated && (
        <div className="an" style={{ whiteSpace: "normal", marginTop: 5 }}>
          Showing the first changed lines only. Apply writes the whole file.
        </div>
      )}
    </>
  );
}
