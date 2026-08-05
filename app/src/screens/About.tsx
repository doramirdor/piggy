// About Piggy.
//
// The one screen that is allowed to make a claim about Piggy rather than about
// the user's tokens, so it holds itself to the same rule as the rest of the app:
// every value in the system table is read from the running build or the disk. A
// version string or a data path that is decorative would be the one fabricated
// number in a product whose whole argument is that it does not fabricate.

import { useEffect, useState } from "react";
import { api } from "../ipc";
import { useStore } from "../store";
import { PiggyMark } from "../components/PiggyMark";
import { APP_VERSION } from "./Settings";
import type { SystemInfo } from "../types";

const NADIR_URL = "https://getnadir.com";
const REPO_URL = "https://github.com/doramirdor/piggy";

const PRINCIPLES = [
  {
    title: "Local-first",
    detail: "Your data stays on this Mac.",
    icon: (
      <>
        <rect x="5" y="10.5" width="14" height="9.5" rx="1" />
        <path d="M8.5 10.5V7.5a3.5 3.5 0 0 1 7 0v3" />
      </>
    ),
  },
  {
    title: "Evidence-first",
    detail: "We only report what we can prove.",
    icon: (
      <>
        <path d="M5 19h14" />
        <path d="M8 19v-6M12 19V8M16 19v-9" />
      </>
    ),
  },
  {
    title: "Reversible",
    detail: "Every change is yours to undo.",
    icon: (
      <>
        <path d="M4.5 11a7.5 7.5 0 1 1 2 6" />
        <path d="M4 6.5V11h4.5" />
      </>
    ),
  },
  {
    title: "Safe by design",
    detail: "We change nothing without your consent.",
    icon: (
      <>
        <path d="M12 3.5 5 6.5V11c0 4.6 3 7.7 7 9.3 4-1.6 7-4.7 7-9.3V6.5l-7-3Z" />
        <path d="m9.3 12 1.9 1.9 3.6-4" />
      </>
    ),
  },
];

const FAMILY = [
  { name: "Nadir", detail: "Routes each request to the appropriate model." },
  { name: "Barber", detail: "Removes unnecessary context before inference." },
  { name: "Piggy", detail: "Measures spend and verifies the resulting savings." },
];

const HELP = [
  { label: "Documentation", url: `${REPO_URL}#readme` },
  { label: "GitHub repository", url: REPO_URL },
  { label: "Report an issue", url: `${REPO_URL}/issues/new` },
  { label: "Contact Nadir Labs", url: NADIR_URL },
];

/** The box-arrow that marks a link as leaving the app. */
function ExternalIcon() {
  return (
    <svg className="ab-ext" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">
      <path d="M13.5 5H19v5.5M19 5l-7.5 7.5" />
      <path d="M17 14.5V19H5V7h4.5" />
    </svg>
  );
}

export function About() {
  const env = useStore((s) => s.env);
  const showError = useStore((s) => s.showError);
  const [sys, setSys] = useState<SystemInfo | null>(null);

  useEffect(() => {
    api.systemInfo().then(setSys).catch((e) => showError(e));
  }, [showError]);

  const open = (url: string) => api.openExternal(url).catch((e) => showError(e));

  // Named from what Piggy is actually reading, not from what it supports: a
  // Claude-only Mac should not see Codex listed as a source.
  const sources = [
    env?.claudeInstalled ? "Claude Code" : null,
    env?.codexInstalled ? "Codex" : null,
  ].filter(Boolean);

  const rows: [string, string][] = [
    ["Version", sys ? sys.version : APP_VERSION],
    ["Architecture", sys?.arch ?? "…"],
    ["Data folder", sys?.dataDir ?? "…"],
    ["Database", sys?.database ?? (sys ? "Not created yet" : "…")],
    ["Session sources", sources.length ? sources.join(", ") : "None found"],
    ["Made by", "Nadir Labs"],
  ];

  return (
    <>
      <div className="head">
        <div>
          <h1>About Piggy</h1>
          <div className="ab-eyebrow">A Nadir Labs product</div>
          <div className="sub">Local-first. Evidence-first. You're in control.</div>
        </div>
      </div>

      <div className="ab-hero">
        <div className="ab-lockup">
          <PiggyMark size={64} />
          <div className="ab-word">Piggy</div>
          <div className="ab-tag">Measure. Save. Prove.</div>
        </div>
        <div className="ab-pitch">
          <p>Piggy is the local-first measurement app from Nadir Labs.</p>
          <p>
            It reads Claude Code and Codex session logs, shows where your tokens went, and uses
            randomised holdouts to verify whether token savers actually worked.
          </p>
          <p className="ab-claim">
            Piggy does not claim a saving until it can show the comparison it measured.
          </p>
        </div>
      </div>

      <div className="ab-principles">
        {PRINCIPLES.map((p) => (
          <div className="ab-principle" key={p.title}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round">
              {p.icon}
            </svg>
            <b>{p.title}</b>
            <span>{p.detail}</span>
          </div>
        ))}
      </div>

      <div className="sect">Part of Nadir Labs</div>
      <div className="rows">
        <div className="ab-family-note">
          Nadir Labs builds tools for more efficient AI systems, from routing and context reduction
          to independent measurement.
        </div>
        {FAMILY.map((f) => (
          <div className="row" key={f.name}>
            <div className="meta">
              <div className="name">{f.name}</div>
              <div className="desc">{f.detail}</div>
            </div>
          </div>
        ))}
      </div>
      <div className="ab-actions">
        <button className="btn wide" onClick={() => open(NADIR_URL)}>
          Explore Nadir Labs
        </button>
        <button className="btn wide" onClick={() => open(REPO_URL)}>
          View Piggy on GitHub
        </button>
      </div>

      <div className="sect">System information</div>
      <div className="rows">
        <div className="ab-sys">
          {rows.map(([label, value]) => (
            <div className="ab-sysrow" key={label}>
              <span className="ab-syslabel">{label}</span>
              <span className="ab-sysval">{value}</span>
            </div>
          ))}
        </div>
        <div className="row">
          <div className="meta">
            <div className="name">Piggy's data folder</div>
            <div className="desc">
              The database, your settings backups, and everything Piggy installed live here.
            </div>
          </div>
          <button className="btn" onClick={() => api.openDataFolder().catch((e) => showError(e))}>
            Open in Finder
          </button>
        </div>
      </div>

      <div className="sect">Get help</div>
      <div className="rows">
        {HELP.map((h) => (
          <button className="ab-link" key={h.label} onClick={() => open(h.url)}>
            <span>{h.label}</span>
            <ExternalIcon />
          </button>
        ))}
      </div>

      <div className="foot-note">
        © 2026 Nadir Labs · Piggy is a Nadir Labs product. No telemetry, no accounts. Piggy uses the
        network only for saver discovery and update checks.
      </div>
    </>
  );
}
