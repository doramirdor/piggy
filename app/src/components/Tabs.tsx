import type { Tab } from "../store";

// Inline SVG glyphs, copied from docs/mockups/panel.html (approved).
const ICONS: Record<Tab, JSX.Element> = {
  home: (
    <svg className="gl" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
      <path d="M3 10.5 12 3l9 7.5" />
      <path d="M5 9.5V21h14V9.5" />
    </svg>
  ),
  dashboard: (
    <svg className="gl" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round">
      <path d="M4 20V10M10 20V4M16 20v-8M21 20H3" />
    </svg>
  ),
  discover: (
    <svg className="gl" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
      <path d="M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M18.4 5.6l-2.1 2.1M7.7 16.3l-2.1 2.1" />
      <circle cx="12" cy="12" r="3.2" />
    </svg>
  ),
  settings: (
    <svg className="gl" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="12" cy="12" r="3" />
      <path d="M19 12a7 7 0 0 0-.1-1.2l2-1.5-2-3.4-2.3 1a7 7 0 0 0-2.1-1.2L14 3h-4l-.5 2.7a7 7 0 0 0-2.1 1.2l-2.3-1-2 3.4 2 1.5A7 7 0 0 0 5 12c0 .4 0 .8.1 1.2l-2 1.5 2 3.4 2.3-1c.6.5 1.4.9 2.1 1.2L10 21h4l.5-2.7a7 7 0 0 0 2.1-1.2l2.3 1 2-3.4-2-1.5c.1-.4.1-.8.1-1.2Z" />
    </svg>
  ),
};

const LABELS: Record<Tab, string> = {
  home: "Home",
  dashboard: "Dashboard",
  discover: "Discover",
  settings: "Settings",
};

const ORDER: Tab[] = ["home", "dashboard", "discover", "settings"];

export function Tabs({ tab, onTab }: { tab: Tab; onTab: (t: Tab) => void }) {
  return (
    <nav className="tabs">
      {ORDER.map((t) => (
        <button key={t} className={`tab ${tab === t ? "active" : ""}`} onClick={() => onTab(t)}>
          {ICONS[t]}
          {LABELS[t]}
        </button>
      ))}
    </nav>
  );
}
