import { useStore } from "../store";
import { SaverIcon } from "../components/SaverIcon";
import { StatusChip } from "../components/StatusChip";
import { statusView, type StatusTone } from "../lib/badge";
import { pctMagnitude } from "../lib/format";
import type { SaverRow } from "../types";

/** Directional trend affordance (not per-point data): rising line for savers
 *  producing savings, wavy dotted for measuring, flat dashed for no data. */
function Trend({ tone }: { tone: StatusTone }) {
  if (tone === "measured" || tone === "estimated") {
    const color = tone === "measured" ? "var(--green-bright)" : "var(--teal)";
    return (
      <svg className="rtrend" viewBox="0 0 60 16" fill="none" aria-hidden>
        <polyline points="2,13 12,11 21,12 30,8 39,9 48,5 58,3" stroke={color} strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />
      </svg>
    );
  }
  if (tone === "measuring") {
    return (
      <svg className="rtrend" viewBox="0 0 60 16" fill="none" aria-hidden>
        <polyline points="2,10 10,7 18,10 26,7 34,10 42,7 50,10 58,7" stroke="var(--accent)" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" strokeDasharray="1 3" />
      </svg>
    );
  }
  return (
    <svg className="rtrend" viewBox="0 0 60 16" fill="none" aria-hidden>
      <line x1="2" y1="8" x2="58" y2="8" stroke="var(--text-3)" strokeWidth="1.8" strokeDasharray="3 4" strokeLinecap="round" />
    </svg>
  );
}

function Row({ saver }: { saver: SaverRow }) {
  const v = statusView(saver.badge);
  const delta = saver.badge.delta;
  const hasSaving = (v.tone === "measured" || v.tone === "estimated") && delta != null;
  return (
    <div className="rrow">
      <div className="rname">
        <SaverIcon id={saver.id} size={34} />
        <div>
          <div className="rn-name">{saver.plainLabel ?? saver.name}</div>
          <div className="rn-desc">{saver.description}</div>
        </div>
      </div>
      <StatusChip badge={saver.badge} />
      <span className={`rsave ${v.tone === "estimated" ? "est" : ""} ${hasSaving ? "" : "none"}`}>
        {hasSaving ? pctMagnitude(delta!) : "—"}
      </span>
      <span className="rnum">{saver.badge.n}</span>
      <span className="col-right">
        <Trend tone={v.tone} />
      </span>
    </div>
  );
}

export function Reports() {
  const savers = useStore((s) => s.savers);
  const rows = savers?.savers ?? [];

  const active = rows.filter((s) => s.enabled).length;
  const measuring = rows.filter((s) => statusView(s.badge).tone === "measuring").length;

  return (
    <>
      <div className="head">
        <div>
          <h1>Reports</h1>
          <div className="sub">Every saver's measured status, savings, and session count.</div>
        </div>
      </div>

      <div className="summary">
        <div className="scard">
          <span className="sic" style={{ background: "rgba(34,197,94,0.14)", color: "#22c55e" }}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
              <circle cx="9" cy="8" r="3.2" />
              <path d="M2.5 20v-1a4.5 4.5 0 0 1 4.5-4.5h4a4.5 4.5 0 0 1 4.5 4.5v1" />
              <path d="M16 5.2a3 3 0 0 1 0 5.6" />
              <path d="M21.5 20v-1a4.5 4.5 0 0 0-3-4.2" />
            </svg>
          </span>
          <div>
            <div className="snum">{rows.length}</div>
            <div className="slabel">savers</div>
          </div>
        </div>
        <div className="scard">
          <span className="sic" style={{ background: "rgba(245,158,11,0.15)", color: "#f59e0b" }}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
              <path d="M3 12h4l2.5 7 5-14 2.5 7H21" />
            </svg>
          </span>
          <div>
            <div className="snum">{active}</div>
            <div className="slabel">active</div>
          </div>
        </div>
        <div className="scard">
          <span className="sic" style={{ background: "rgba(59,130,246,0.14)", color: "#3b82f6" }}>
            <span className="chip-spinner" style={{ width: 18, height: 18, borderWidth: 2.4 }} />
          </span>
          <div>
            <div className="snum">{measuring}</div>
            <div className="slabel">measuring</div>
          </div>
        </div>
        <div className="auto-note">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
            <circle cx="12" cy="12" r="9" />
            <path d="M12 11v5" />
            <circle cx="12" cy="8" r="0.5" fill="currentColor" />
          </svg>
          Measurement updates automatically
        </div>
      </div>

      <div className="rtable">
        <div className="rhead">
          <span>Saver</span>
          <span>Status</span>
          <span>Savings</span>
          <span>Sessions</span>
          <span className="col-right">Trend</span>
        </div>
        {rows.map((s) => (
          <Row key={s.id} saver={s} />
        ))}
        {rows.length === 0 && (
          <div className="rrow" style={{ gridTemplateColumns: "1fr" }}>
            <span className="rnum">No savers yet.</span>
          </div>
        )}
      </div>

      <div className="foot-note">
        Savings are shown once holdout data is strong enough. Trend is directional, not a
        per-session series.
      </div>
    </>
  );
}
