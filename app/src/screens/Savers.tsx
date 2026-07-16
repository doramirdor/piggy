import { useState } from "react";
import { useStore } from "../store";
import { Switch } from "../components/Switch";
import { StatusChip } from "../components/StatusChip";
import { SaverIcon } from "../components/SaverIcon";
import { SaverConfigPanel } from "../components/SaverConfig";
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
    <button type="button" className="info-btn" title={info} aria-label={`About ${saver.name}`}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
        <circle cx="12" cy="12" r="9" />
        <path d="M12 11v5" />
        <circle cx="12" cy="8" r="0.5" fill="currentColor" />
      </svg>
    </button>
  );
}

function GearBtn({
  name,
  open,
  onClick,
}: {
  name: string;
  open: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      className={`info-btn gear ${open ? "open" : ""}`}
      title={`Configure ${name}`}
      aria-label={`Configure ${name}`}
      aria-expanded={open}
      onClick={onClick}
    >
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
        <circle cx="12" cy="12" r="3.1" />
        <path d="M12 2.8v2.4M12 18.8v2.4M2.8 12h2.4M18.8 12h2.4M5.5 5.5l1.7 1.7M16.8 16.8l1.7 1.7M18.5 5.5l-1.7 1.7M7.2 16.8l-1.7 1.7" />
      </svg>
    </button>
  );
}

function SaverRowItem({ saver }: { saver: SaverRow }) {
  const busy = useStore((s) => s.busySavers.includes(saver.id));
  const toggle = useStore((s) => s.toggleSaver);
  const [configOpen, setConfigOpen] = useState(false);
  const name = saver.name;
  return (
    <>
      <div className="row">
        <SaverIcon id={saver.id} />
        <div className="meta">
          <div className="name">
            {name}
            {saver.behaviorChanging && <WarnTri text={saver.warning} />}
          </div>
          {busy ? (
            <>
              <div className="desc applying">{saver.enabled ? "Turning off…" : "Turning on…"}</div>
              <div className="progress row-progress" role="progressbar" aria-label={`${name} updating`}>
                <div className="progress-bar" />
              </div>
            </>
          ) : (
            <div className="desc">{saver.description}</div>
          )}
          {saver.licenseNote && (
            <div className="lic-note">
              <span className="lic-badge">{saver.license}</span>
              <span>{saver.licenseNote}</span>
            </div>
          )}
        </div>
        <StatusChip badge={saver.badge} />
        {saver.configurable && (
          <GearBtn name={name} open={configOpen} onClick={() => setConfigOpen((o) => !o)} />
        )}
        <InfoBtn saver={saver} />
        <Switch
          on={saver.enabled}
          busy={busy}
          onChange={(next) => toggle(saver.id, next)}
          label={`Turn ${name} ${saver.enabled ? "off" : "on"}`}
        />
      </div>
      {configOpen && saver.configurable && <SaverConfigPanel saverId={saver.id} />}
    </>
  );
}

export function Savers() {
  const savers = useStore((s) => s.savers);
  const masterOn = savers?.masterOn ?? false;
  const masterBusy = useStore((s) => s.masterBusy);
  const toggleMaster = useStore((s) => s.toggleMaster);

  // While the switch is in flight, savers.masterOn still holds the *old* value,
  // so the direction of travel is simply the opposite of the current state.
  const turningOn = masterBusy && !masterOn;

  return (
    <>
      <div className="head">
        <div>
          <h1>Savers</h1>
          <div className="sub">Every change is local, measured, and reversible.</div>
        </div>
      </div>

      <div
        className={`master ${masterBusy ? `busy ${turningOn ? "turning-on" : "turning-off"}` : masterOn ? "on" : "off"}`}
      >
        <div className="mpower">
          {masterBusy && <span className="mpower-ring" aria-hidden />}
          <svg width="26" height="26" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden>
            <path d="M12 3v9" />
            <path d="M7 6.3a7.5 7.5 0 1 0 10 0" />
          </svg>
        </div>
        {masterBusy ? (
          <div className="txt">
            <div className="t1">{turningOn ? "Turning Piggy on…" : "Turning Piggy off…"}</div>
            <div className="t2">
              {turningOn
                ? "Waking your savers and applying their config."
                : "Pausing your savers and reverting their changes."}
            </div>
            <div
              className="progress master-progress"
              role="progressbar"
              aria-label={turningOn ? "Turning Piggy on" : "Turning Piggy off"}
            >
              <div className="progress-bar" />
            </div>
          </div>
        ) : (
          <div className="txt">
            <div className="t1">{masterOn ? "Piggy is ON" : "Piggy is OFF"}</div>
            <div className="t2">
              {masterOn
                ? "Piggy is running. Your enabled savers are live."
                : "All savers are paused. No changes are active."}
            </div>
            <div className="mstatus">
              <i />
              {masterOn ? "Active and measuring" : "No impact while off"}
            </div>
          </div>
        )}
        <Switch on={masterOn} busy={masterBusy} onChange={toggleMaster} label="Piggy master switch" />
      </div>

      <div className="sect-head">
        <h2>All savers</h2>
        <span className="helper">Fine-tune how Claude works to save tokens.</span>
      </div>
      <div className="rows">
        {savers === null ? (
          <div className="row">
            <div className="meta">
              <div className="desc">Loading savers…</div>
              <div className="progress" role="progressbar" aria-label="Loading savers">
                <div className="progress-bar" />
              </div>
            </div>
          </div>
        ) : savers.savers.length === 0 ? (
          <div className="row">
            <div className="desc">No savers available yet.</div>
          </div>
        ) : (
          savers.savers.map((s) => <SaverRowItem key={s.id} saver={s} />)
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
