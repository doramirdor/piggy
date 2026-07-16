import { useEffect, useState } from "react";
import { api } from "../ipc";
import { useStore } from "../store";
import { formatTokens } from "../lib/format";
import type { SweepReport } from "../types";

const KIND_LABEL: Record<string, string> = {
  mcp: "MCP server",
  plugin: "plugin",
  skill: "skill",
  hook: "hook",
};

/** Modal listing Sweep's unused add-ons, with reversible one-click turn-off. */
export function SweepSheet({ onClose }: { onClose: () => void }) {
  const [report, setReport] = useState<SweepReport | null>(null);
  const [busy, setBusy] = useState(false);
  const showError = useStore((s) => s.showError);

  const load = async () => {
    try {
      setReport(await api.sweepReport());
    } catch (e) {
      showError(e);
    }
  };
  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const apply = async (stableId: string) => {
    setBusy(true);
    try {
      setReport(await api.sweepApply([stableId]));
    } catch (e) {
      showError(e);
    } finally {
      setBusy(false);
    }
  };

  const undoAll = async () => {
    setBusy(true);
    try {
      setReport(await api.sweepRestore([]));
    } catch (e) {
      showError(e);
    } finally {
      setBusy(false);
    }
  };

  const recommended = report?.items.filter((i) => i.recommendDisable) ?? [];
  const inUse = report?.items.filter((i) => !i.recommendDisable) ?? [];

  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <div className="sheet" onClick={(e) => e.stopPropagation()}>
        <div className="stitle">Unused extras</div>

        {report && (
          <div style={{ fontSize: 11, color: "var(--text-2)", marginBottom: 8, whiteSpace: "normal", lineHeight: 1.4 }}>
            {recommended.length > 0
              ? `${recommended.length} add-on${recommended.length === 1 ? "" : "s"} you never use cost ~${formatTokens(report.estRecoverableTokens)} tokens per request. Token costs are estimated.`
              : "Everything here is in use - nothing to sweep."}
          </div>
        )}

        <div className="attr">
          {recommended.map((i) => (
            <div className="arow" key={i.stableId}>
              <div style={{ flex: 1, minWidth: 0 }}>
                <div className="aname">{i.id}</div>
                <div className="an" style={{ whiteSpace: "normal" }}>
                  {KIND_LABEL[i.kind] ?? i.kind} · ~{formatTokens(i.estTokens)} tokens · {i.reason}
                </div>
              </div>
              <button className="btn" disabled={busy} onClick={() => apply(i.stableId)}>
                Turn off
              </button>
            </div>
          ))}
          {recommended.length === 0 && (
            <div className="arow">
              <div className="an" style={{ whiteSpace: "normal" }}>
                Nothing to turn off right now.
              </div>
            </div>
          )}
        </div>

        {inUse.length > 0 && (
          <>
            <div className="sect" style={{ paddingLeft: 4 }}>
              In use - kept
            </div>
            <div className="attr">
              {inUse.map((i) => (
                <div className="arow" key={i.stableId}>
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div className="aname">{i.id}</div>
                    <div className="an" style={{ whiteSpace: "normal" }}>
                      {KIND_LABEL[i.kind] ?? i.kind} · {i.reason}
                    </div>
                  </div>
                </div>
              ))}
            </div>
          </>
        )}

        <div className="sactions">
          <button className="btn" disabled={busy} onClick={undoAll}>
            Undo all
          </button>
          <button className="btn primary" onClick={onClose}>
            Done
          </button>
        </div>
      </div>
    </div>
  );
}
