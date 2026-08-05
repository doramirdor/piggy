import { useEffect } from "react";
import { useStore } from "./store";
import { onStatsUpdated } from "./ipc";
import { Sidebar } from "./components/Sidebar";
import { Banner } from "./components/Banner";
import { Ledger } from "./screens/Ledger";
import { Savers } from "./screens/Savers";
import { Proof } from "./screens/Proof";
import { Settings } from "./screens/Settings";
import { About } from "./screens/About";
import { NoClaude, FirstRun } from "./screens/EmptyStates";
import { PiggyMark } from "./components/PiggyMark";
import { usePageTurn } from "./lib/motion";

export default function App() {
  const booting = useStore((s) => s.booting);
  const env = useStore((s) => s.env);
  const tab = useStore((s) => s.tab);
  const setTab = useStore((s) => s.setTab);
  const boot = useStore((s) => s.boot);
  const refresh = useStore((s) => s.refresh);
  // Hoisted above the early returns below: hooks must run in the same order on
  // every render, and this component bails out for the booting, no-tool and
  // first-run states before it reaches the JSX.
  const turnKey = usePageTurn(tab);

  // Boot once.
  useEffect(() => {
    void boot();
  }, [boot]);

  // Re-query on the background index event and whenever the window regains focus.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onStatsUpdated(() => void refresh()).then((u) => (unlisten = u));

    const onVisible = () => {
      if (document.visibilityState === "visible") void refresh();
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      unlisten?.();
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [refresh]);

  // Full-bleed states (no sidebar): booting progress, no-Claude, first-run.
  if (booting) {
    return (
      <div className="empty">
        <PiggyMark size={56} className="mark" />
        <div className="progress" role="progressbar" aria-label="Loading Piggy">
          <div className="progress-bar" />
        </div>
      </div>
    );
  }
  // Full-bleed only when NEITHER tool is present; a Codex-only Mac still gets
  // the real app (observability works there, savers just have nothing to hook).
  if (env && !env.claudeInstalled && !env.codexInstalled) return <NoClaude />;
  if (env && !env.hasData) return <FirstRun />;

  const screen =
    tab === "spend" ? <Ledger />
    : tab === "savers" ? <Savers />
    : tab === "proof" ? <Proof />
    : tab === "about" ? <About />
    : <Settings />;

  return (
    <div className="win">
      <Sidebar tab={tab} onTab={setTab} />
      <main className="content">
        <div className="inner">
          <Banner />
          {/* THE PAGE. The key changes with the tab, so React replaces the
              subtree and the CSS stagger re-runs: a page turning rather than a
              panel sliding. `usePageTurn` returns a constant under
              prefers-reduced-motion so the animation never arms. */}
          <div className="page-turn" key={turnKey}>
            {screen}
          </div>
        </div>
      </main>
    </div>
  );
}
