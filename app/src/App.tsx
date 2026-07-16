import { useEffect } from "react";
import { useStore } from "./store";
import { onStatsUpdated } from "./ipc";
import { Sidebar } from "./components/Sidebar";
import { Banner } from "./components/Banner";
import { Overview } from "./screens/Overview";
import { Savers } from "./screens/Savers";
import { Discover } from "./screens/Discover";
import { Proof } from "./screens/Proof";
import { Reports } from "./screens/Reports";
import { Settings } from "./screens/Settings";
import { NoClaude, FirstRun } from "./screens/EmptyStates";
import { PiggyMark } from "./components/PiggyMark";

export default function App() {
  const booting = useStore((s) => s.booting);
  const env = useStore((s) => s.env);
  const tab = useStore((s) => s.tab);
  const setTab = useStore((s) => s.setTab);
  const boot = useStore((s) => s.boot);
  const refresh = useStore((s) => s.refresh);

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
    tab === "overview" ? (
      <Overview />
    ) : tab === "savers" ? (
      <Savers />
    ) : tab === "discover" ? (
      <Discover />
    ) : tab === "proof" ? (
      <Proof />
    ) : tab === "reports" ? (
      <Reports />
    ) : (
      <Settings />
    );

  return (
    <div className="win">
      <Sidebar tab={tab} onTab={setTab} />
      <main className="content">
        <div className="inner">
          <Banner />
          {screen}
        </div>
      </main>
    </div>
  );
}
