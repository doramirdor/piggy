import { useEffect } from "react";
import { useStore } from "./store";
import { onStatsUpdated, hidePanel } from "./ipc";
import { Header } from "./components/Header";
import { Banner } from "./components/Banner";
import { Tabs } from "./components/Tabs";
import { Home } from "./screens/Home";
import { Dashboard } from "./screens/Dashboard";
import { Discover } from "./screens/Discover";
import { Settings } from "./screens/Settings";
import { NoClaude, FirstRun } from "./screens/EmptyStates";

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

  // Re-query on the background index event and whenever the panel becomes visible
  // (the popover is shown), matching the "refresh on window-show" spec.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    onStatsUpdated(() => void refresh()).then((u) => (unlisten = u));

    const onVisible = () => {
      if (document.visibilityState === "visible") void refresh();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") void hidePanel();
    };
    document.addEventListener("visibilitychange", onVisible);
    window.addEventListener("keydown", onKey);
    return () => {
      unlisten?.();
      document.removeEventListener("visibilitychange", onVisible);
      window.removeEventListener("keydown", onKey);
    };
  }, [refresh]);

  let body: JSX.Element;
  if (booting) {
    body = (
      <div className="empty">
        <div className="spinner" />
      </div>
    );
  } else if (env && !env.claudeInstalled) {
    body = <NoClaude />;
  } else if (env && !env.hasData) {
    body = <FirstRun />;
  } else {
    body =
      tab === "home" ? (
        <Home />
      ) : tab === "dashboard" ? (
        <Dashboard />
      ) : tab === "discover" ? (
        <Discover />
      ) : (
        <Settings />
      );
  }

  const showChrome = !booting && env != null && env.claudeInstalled;

  return (
    <div className="app-shell">
      <Header />
      <Banner />
      {body}
      {showChrome && <Tabs tab={tab} onTab={setTab} />}
    </div>
  );
}
