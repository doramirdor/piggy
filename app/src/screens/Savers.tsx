import { useStore } from "../store";
import { Switch } from "../components/Switch";
import { StatusChip } from "../components/StatusChip";
import { SaverIcon } from "../components/SaverIcon";
import type { SaverRow } from "../types";

function WarnTri({ text }: { text: string | null }) {
  return (
    <svg
      className="warn-tri"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.9"
      strokeLinecap="round"
      strokeLinejoin="round"
      role="img"
      aria-label={text ?? "Changes how Claude behaves"}
    >
      <title>{text ?? "Changes how Claude behaves"}</title>
      <path d="M12 3.6 22 20.5H2z" />
      <path d="M12 10v4.5" />
      <circle cx="12" cy="17.6" r="0.4" fill="currentColor" />
    </svg>
  );
}

function InfoBtn({ saver }: { saver: SaverRow }) {
  const info = saver.warning ?? saver.claimedSavings ?? saver.description;
  return (
    <button type="button" className="info-btn" title={info} aria-label={`About ${saver.plainLabel ?? saver.name}`}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
        <circle cx="12" cy="12" r="9" />
        <path d="M12 11v5" />
        <circle cx="12" cy="8" r="0.5" fill="currentColor" />
      </svg>
    </button>
  );
}

function SaverRowItem({ saver }: { saver: SaverRow }) {
  const busy = useStore((s) => s.busySavers.includes(saver.id));
  const toggle = useStore((s) => s.toggleSaver);
  const name = saver.plainLabel ?? saver.name;
  return (
    <div className="row">
      <SaverIcon id={saver.id} />
      <div className="meta">
        <div className="name">
          {name}
          {saver.behaviorChanging && <WarnTri text={saver.warning} />}
        </div>
        <div className="desc">{saver.description}</div>
      </div>
      <StatusChip badge={saver.badge} />
      <InfoBtn saver={saver} />
      <Switch
        on={saver.enabled}
        busy={busy}
        onChange={(next) => toggle(saver.id, next)}
        label={`Turn ${name} ${saver.enabled ? "off" : "on"}`}
      />
    </div>
  );
}

export function Savers() {
  const savers = useStore((s) => s.savers);
  const masterOn = savers?.masterOn ?? false;
  const masterBusy = useStore((s) => s.masterBusy);
  const toggleMaster = useStore((s) => s.toggleMaster);

  return (
    <>
      <div className="head">
        <div>
          <h1>Savers</h1>
          <div className="sub">Every change is local, measured, and reversible.</div>
        </div>
      </div>

      <div className={`master ${masterOn ? "on" : "off"}`}>
        <div className="mpower">
          <svg width="26" height="26" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
            <path d="M12 3v9" />
            <path d="M7 6.3a7.5 7.5 0 1 0 10 0" />
          </svg>
        </div>
        <div className="txt">
          <div className="t1">{masterOn ? "Piggy is ON" : "Piggy is OFF"}</div>
          <div className="t2">
            {masterOn
              ? "All savers are active. Changes are live."
              : "All savers are paused. No changes are active."}
          </div>
          <div className="mstatus">
            <i />
            {masterOn ? "Active and measuring" : "No impact while off"}
          </div>
        </div>
        <Switch on={masterOn} busy={masterBusy} onChange={toggleMaster} label="Piggy master switch" />
      </div>

      <div className="sect-head">
        <h2>All savers</h2>
        <span className="helper">Fine-tune how Claude works to save tokens.</span>
      </div>
      <div className="rows">
        {savers?.savers.map((s) => (
          <SaverRowItem key={s.id} saver={s} />
        ))}
        {(!savers || savers.savers.length === 0) && (
          <div className="row">
            <div className="desc">No savers available yet.</div>
          </div>
        )}
      </div>

      <div className="safe-note">
        <span className="shield">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
            <path d="M12 3 5 6v5c0 4.5 3 7.6 7 9 4-1.4 7-4.5 7-9V6z" />
            <path d="M9 12l2 2 4-4" />
          </svg>
        </span>
        <span>
          <b>Safe by design.</b> You're in control. Turn on what you want, anytime.
        </span>
      </div>
    </>
  );
}
