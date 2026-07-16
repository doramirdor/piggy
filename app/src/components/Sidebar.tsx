import type { Tab } from "../store";
import { useStore } from "../store";
import { APP_VERSION } from "../screens/Settings";
import { PiggyMark } from "./PiggyMark";
import { Switch } from "./Switch";

// Line-icon set for the sidebar nav (SF-Symbols-adjacent, 1.7px stroke).
const ICONS: Record<Tab, JSX.Element> = {
  overview: (
    <svg className="ni-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
      <path d="M3 10.5 12 3l9 7.5" />
      <path d="M5 9.5V21h14V9.5" />
    </svg>
  ),
  savers: (
    <svg className="ni-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
      <path d="M4 7h16M4 12h16M4 17h16" />
      <circle cx="9" cy="7" r="1.6" fill="currentColor" stroke="none" />
      <circle cx="15" cy="17" r="1.6" fill="currentColor" stroke="none" />
    </svg>
  ),
  discover: (
    <svg className="ni-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="11" cy="11" r="7" />
      <path d="m20 20-3.2-3.2" />
    </svg>
  ),
  proof: (
    <svg className="ni-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
      <path d="M9 12.5 11 14.5 15.5 10" />
      <path d="M12 3 4 6.5V11c0 5 3.4 8.3 8 10 4.6-1.7 8-5 8-10V6.5L12 3Z" />
    </svg>
  ),
  reports: (
    <svg className="ni-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
      <path d="M4 20V10M10 20V4M16 20v-8M21 20H3" />
    </svg>
  ),
  settings: (
    <svg className="ni-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="12" cy="12" r="3" />
      <path d="M19 12a7 7 0 0 0-.1-1.2l2-1.5-2-3.4-2.3 1a7 7 0 0 0-2.1-1.2L14 3h-4l-.5 2.7a7 7 0 0 0-2.1 1.2l-2.3-1-2 3.4 2 1.5A7 7 0 0 0 5 12c0 .4 0 .8.1 1.2l-2 1.5 2 3.4 2.3-1c.6.5 1.4.9 2.1 1.2L10 21h4l.5-2.7a7 7 0 0 0 2.1-1.2l2.3 1 2-3.4-2-1.5c.1-.4.1-.8.1-1.2Z" />
    </svg>
  ),
};

const LABELS: Record<Tab, string> = {
  overview: "Dashboard",
  savers: "Savers",
  discover: "Discovery",
  proof: "Proof",
  reports: "Reports",
  settings: "Settings",
};

const ORDER: Tab[] = ["overview", "savers", "discover", "proof", "reports", "settings"];

export function Sidebar({ tab, onTab }: { tab: Tab; onTab: (t: Tab) => void }) {
  const savers = useStore((s) => s.savers);
  const masterOn = savers?.masterOn ?? false;
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
          >
            {ICONS[t]}
            <span className="ni-label">{LABELS[t]}</span>
          </button>
        ))}
      </nav>
      <div className="foot">
        <div className="master-mini">
          <div className="mtxt">
            <div className="m1">{masterOn ? "Piggy is ON" : "Piggy is OFF"}</div>
            <div className="m2">{masterOn ? "Your savers are live" : "Savers are paused"}</div>
          </div>
          <Switch on={masterOn} busy={masterBusy} onChange={toggleMaster} label="Piggy master switch" />
        </div>
        <div className="version">v{APP_VERSION}</div>
      </div>
    </aside>
  );
}
