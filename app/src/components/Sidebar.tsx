import type { Tab } from "../store";
import { useStore } from "../store";
import { APP_VERSION } from "../screens/Settings";
import { PiggyMark } from "./PiggyMark";
import { Switch } from "./Switch";

// Line-icon set for the sidebar nav (SF-Symbols-adjacent, 1.7px stroke).
const ICONS: Record<Tab, JSX.Element> = {
  spend: (
    <svg className="ni-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
      <path d="M4 5h16v14H4z" />
      <path d="M4 9.5h16M9.5 9.5V19" />
    </svg>
  ),
  savers: (
    <svg className="ni-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
      <path d="M4 7h16M4 12h16M4 17h16" />
      <circle cx="9" cy="7" r="1.6" fill="currentColor" stroke="none" />
      <circle cx="15" cy="17" r="1.6" fill="currentColor" stroke="none" />
    </svg>
  ),
  proof: (
    <svg className="ni-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
      <path d="M9 12.5 11 14.5 15.5 10" />
      <path d="M12 3 4 6.5V11c0 5 3.4 8.3 8 10 4.6-1.7 8-5 8-10V6.5L12 3Z" />
    </svg>
  ),
  settings: (
    <svg className="ni-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="12" cy="12" r="3" />
      <path d="M19 12a7 7 0 0 0-.1-1.2l2-1.5-2-3.4-2.3 1a7 7 0 0 0-2.1-1.2L14 3h-4l-.5 2.7a7 7 0 0 0-2.1 1.2l-2.3-1-2 3.4 2 1.5A7 7 0 0 0 5 12c0 .4 0 .8.1 1.2l-2 1.5 2 3.4 2.3-1c.6.5 1.4.9 2.1 1.2L10 21h4l.5-2.7a7 7 0 0 0 2.1-1.2l2.3 1 2-3.4-2-1.5c.1-.4.1-.8.1-1.2Z" />
    </svg>
  ),
  about: (
    <svg className="ni-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="12" cy="12" r="9" />
      <path d="M12 11v5.5" />
      <circle cx="12" cy="7.9" r="1" fill="currentColor" stroke="none" />
    </svg>
  ),
};

const LABELS: Record<Tab, string> = {
  spend: "Spend",
  savers: "Savers",
  proof: "Proof",
  settings: "Settings",
  about: "About Piggy",
};

const ORDER: Tab[] = ["spend", "savers", "proof", "about", "settings"];

export function Sidebar({ tab, onTab }: { tab: Tab; onTab: (t: Tab) => void }) {
  const savers = useStore((s) => s.savers);
  const masterOn = savers?.masterOn ?? false;
  // The master switch can read ON while every saver is off. Don't claim savers
  // are live unless at least one actually is, or this pill contradicts the
  // dashboard hero (which keys off enabled savers, not the master flag).
  const anyEnabled = (savers?.savers ?? []).some((s) => s.enabled);
  const masterBusy = useStore((s) => s.masterBusy);
  const toggleMaster = useStore((s) => s.toggleMaster);

  return (
    <aside className="sidebar">
      <div className="brand">
        <PiggyMark size={22} />
        <span>Piggy</span>
      </div>
      <div className="tagline">Measure. Save. Prove.</div>
      <nav>
        {ORDER.map((t) => (
          <button
            key={t}
            className={`nav-item ${tab === t ? "active" : ""}`}
            onClick={() => onTab(t)}
            aria-current={tab === t ? "page" : undefined}
            // The visible label disappears below 720px (index.css hides
            // .ni-label), which would leave a screen reader an unnamed
            // icon-only button; the aria-label survives the collapse.
            aria-label={LABELS[t]}
          >
            {ICONS[t]}
            <span className="ni-label">{LABELS[t]}</span>
          </button>
        ))}
      </nav>
      <div className="foot">
        {/* "Piggy is OFF" was wrong: the app is open and still reading
            sessions. What is off is the saver system, and measurement carries
            on regardless. Conflating the two told the user measurement had
            stopped when it had not, on the one screen that exists to say
            what is being measured. */}
        <div className="master-mini">
          <span className={`mstate ${masterOn && anyEnabled ? "on" : "off"}`} aria-hidden />
          <div className="mtxt">
            <div className="m1">
              {savers === null
                ? "Checking savers…"
                : masterOn
                  ? anyEnabled
                    ? "Savers on"
                    : "No savers on"
                  : "Savers are off"}
            </div>
            <div className="m2">Measurement continues</div>
          </div>
          <Switch
            on={masterOn}
            busy={masterBusy}
            disabled={savers === null}
            onChange={toggleMaster}
            label="Turn savers on or off"
          />
        </div>
        <div className="version">v{APP_VERSION}</div>
      </div>
    </aside>
  );
}
