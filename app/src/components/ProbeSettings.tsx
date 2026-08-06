import { useEffect, useState } from "react";
import { api } from "../ipc";
import { useStore } from "../store";
import { basisClass, basisLabel } from "../lib/advice";
import { probeRow } from "../lib/probe";
import type { ProbeReport, ProbeServer } from "../types";

/** Identity of one row: the same server name can sit at user scope and inside a
 *  project with different arguments, and they are different measurements. */
function rowKey(s: ProbeServer): string {
  return `${s.key}|${s.scope}`;
}

/**
 * Measuring what an MCP server actually costs.
 *
 * Sweep has always guessed a server's context cost from the size of its config,
 * and said so. The only way to know is to start the server and read the tool
 * list it sends: the same list Claude Code loads into every session. Piggy will
 * do that, once, when you ask it to, one server at a time, and never on its own.
 *
 * The figures that come back do not share one label, and that is the point: the
 * schema bytes are counted, and turning them into tokens is a division by 3.5
 * until a real tokenizer ships.
 */
export function ProbeSettings() {
  const showError = useStore((s) => s.showError);
  const [report, setReport] = useState<ProbeReport | null>(null);
  const [busyKey, setBusyKey] = useState<string | null>(null);

  useEffect(() => {
    api
      .probeReport()
      .then(setReport)
      .catch((e) => showError(e));
  }, [showError]);

  if (!report || report.servers.length === 0) return null;

  const measure = async (s: ProbeServer) => {
    setBusyKey(rowKey(s));
    try {
      setReport(await api.probeMeasure(s.key, s.scope));
    } catch (e) {
      showError(e);
    } finally {
      setBusyKey(null);
    }
  };

  return (
    <>
      <div className="sect">MCP servers</div>
      <div className="foot-note" style={{ marginTop: 0 }}>
        Measuring starts the server once, reads the list of tools it offers, and stops it. These
        are commands Claude Code already runs in every session, so nothing new is being trusted;
        Piggy just will not run one without you asking.
      </div>
      <div className="rows">
        {report.servers.map((s) => {
          const view = probeRow(s);
          const key = rowKey(s);
          const busy = busyKey === key;
          return (
            <div className="setrow" key={key}>
              <div className="smeta">
                <div className="sname">
                  {s.key} <span className="probe-scope">{s.scopeLabel}</span>
                </div>
                <div className="sdesc">{view.note}</div>
                {view.figures.length > 0 && (
                  <div className="probe-figs">
                    {view.figures.map((f) => (
                      <span key={f.label}>
                        {f.value} {f.label}{" "}
                        <span className={`lins-tag ${basisClass(f.basis)}`}>
                          {basisLabel(f.basis)}
                        </span>
                      </span>
                    ))}
                  </div>
                )}
              </div>
              {view.action && (
                <button
                  className="btn"
                  disabled={busyKey !== null}
                  onClick={() => void measure(s)}
                >
                  {busy ? "Measuring…" : view.action}
                </button>
              )}
            </div>
          );
        })}
      </div>
      <div className="foot-note">
        Schema bytes are measured. What a session is charged for them is estimated, so anything
        monthly on an advice card says so.
      </div>
    </>
  );
}
