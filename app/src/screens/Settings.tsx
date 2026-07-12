import { useEffect, useState } from "react";
import { api } from "../ipc";
import { useStore } from "../store";
import { Switch } from "../components/Switch";
import type { Doctor, Settings as SettingsData } from "../types";

const APP_VERSION = "0.1.0";

export function Settings() {
  const showError = useStore((s) => s.showError);
  const refresh = useStore((s) => s.refresh);
  const [settings, setSettings] = useState<SettingsData | null>(null);
  const [doctor, setDoctor] = useState<Doctor | null>(null);
  const [confirmRestore, setConfirmRestore] = useState(false);
  const [restoreMsg, setRestoreMsg] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.settingsGet().then(setSettings).catch((e) => showError(e));
    api.doctor().then(setDoctor).catch((e) => showError(e));
  }, [showError]);

  const commit = async (next: SettingsData) => {
    setSettings(next);
    try {
      setSettings(await api.settingsSet(next));
    } catch (e) {
      showError(e);
    }
  };

  const restore = async () => {
    setBusy(true);
    try {
      const res = await api.restoreDefaults();
      setRestoreMsg(
        res.byteRestored
          ? "Your Claude settings are back exactly as before Piggy."
          : "Piggy's changes were undone.",
      );
      setConfirmRestore(false);
      await refresh();
    } catch (e) {
      showError(e);
    } finally {
      setBusy(false);
    }
  };

  const settingsHead = (
    <div className="head">
      <div>
        <h1>Settings</h1>
        <div className="sub">Keep Piggy private, honest, and reversible.</div>
      </div>
    </div>
  );

  if (!settings) {
    return settingsHead;
  }

  const holdoutPct = Math.round(settings.holdoutFraction * 100);

  return (
    <>
      {settingsHead}

      <div className="rows">
        <div className="setrow">
          <div className="smeta">
            <div className="sname">Holdout for measuring</div>
            <div className="sdesc">
              Piggy runs {holdoutPct}% of sessions with savers off, so it can prove real savings.
            </div>
          </div>
          <input
            className="slider"
            type="range"
            min={0}
            max={30}
            step={5}
            value={holdoutPct}
            onChange={(e) => setSettings({ ...settings, holdoutFraction: Number(e.target.value) / 100 })}
            onPointerUp={() => commit(settings)}
            onKeyUp={() => commit(settings)}
          />
          <span className="val">{holdoutPct}%</span>
        </div>

        <div className="setrow">
          <div className="smeta">
            <div className="sname">Rotate savers for fair tests</div>
            <div className="sdesc">Alternates which savers run so each gets measured honestly.</div>
          </div>
          <Switch
            on={settings.rotationEnabled}
            onChange={(v) => commit({ ...settings, rotationEnabled: v })}
            label="Rotate savers"
          />
        </div>

        <div className="setrow">
          <div className="smeta">
            <div className="sname">Open Piggy at login</div>
            <div className="sdesc">Starts Piggy automatically so your savings keep running.</div>
          </div>
          <Switch
            on={settings.launchAtLogin}
            onChange={(v) => commit({ ...settings, launchAtLogin: v })}
            label="Open at login"
          />
        </div>
      </div>

      <div className="sect">Health</div>
      <div className="rows">
        {doctor?.checks.map((c) => (
          <div className="health" key={c.label}>
            <span className="hmark">{c.ok ? "✅" : "⚠️"}</span>
            <div className="hmeta">
              <div className="hlabel">{c.label}</div>
              <div className="hdetail">{c.detail}</div>
            </div>
          </div>
        ))}
        {!doctor && <div className="health"><div className="hdetail">Checking…</div></div>}
      </div>

      <div className="sect">Reset</div>
      <button className="btn danger wide" onClick={() => setConfirmRestore(true)}>
        Restore Defaults
      </button>
      {restoreMsg && <div className="foot-note">{restoreMsg}</div>}

      <div className="foot-note">
        Piggy {APP_VERSION} · Piggy never phones home. Network is used only to fetch savers from
        GitHub.
      </div>

      {confirmRestore && (
        <div className="sheet-backdrop" onClick={() => setConfirmRestore(false)}>
          <div className="sheet" onClick={(e) => e.stopPropagation()}>
            <div className="stitle">Restore defaults?</div>
            <div style={{ fontSize: 12, color: "var(--text-2)", whiteSpace: "normal", lineHeight: 1.45 }}>
              This puts Claude's settings back exactly as before Piggy and turns every saver off.
              You can turn them back on anytime.
            </div>
            <div className="sactions">
              <button className="btn wide" onClick={() => setConfirmRestore(false)}>
                Cancel
              </button>
              <button className="btn wide danger" disabled={busy} onClick={restore}>
                Restore
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
