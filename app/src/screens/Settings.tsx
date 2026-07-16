import { useEffect, useState } from "react";
import { api } from "../ipc";
import { useStore } from "../store";
import { Switch } from "../components/Switch";
import type { Doctor, Settings as SettingsData, UpdateInfo } from "../types";

// Single source of truth for the version shown in the UI (the sidebar imports it
// too). Bump this in step with tauri.conf.json / Cargo.toml / package.json.
export const APP_VERSION = "0.1.0";

export function Settings() {
  const showError = useStore((s) => s.showError);
  const refresh = useStore((s) => s.refresh);
  const [settings, setSettings] = useState<SettingsData | null>(null);
  const [doctor, setDoctor] = useState<Doctor | null>(null);
  const [confirmRestore, setConfirmRestore] = useState(false);
  const [restoreMsg, setRestoreMsg] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  const [checking, setChecking] = useState(false);
  const [updateNote, setUpdateNote] = useState<string | null>(null);

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

  // Checking is manual and reports inline: an update endpoint that is briefly
  // unreachable is not worth a modal error banner over the whole app.
  const checkUpdate = async () => {
    setChecking(true);
    setUpdateNote(null);
    try {
      const found = await api.checkForUpdate();
      setUpdate(found);
      if (!found) setUpdateNote(`Piggy ${APP_VERSION} is the latest version.`);
    } catch (e) {
      const detail = (e as { detail?: string })?.detail;
      setUpdateNote(detail ? `Couldn't check for updates: ${detail}` : "Couldn't check for updates.");
    } finally {
      setChecking(false);
    }
  };

  const installUpdate = async () => {
    setBusy(true);
    try {
      // On success the app relaunches into the new build and never returns here.
      await api.installUpdate();
    } catch (e) {
      showError(e);
      setBusy(false);
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

        <div className="setrow">
          <div className="smeta">
            <div className="sname">Command line tool</div>
            <div className="sdesc">
              Adds the <code>piggy</code> command to your terminal for stats and reports. Piggy
              links it into ~/.piggy/bin and adds that folder to your PATH in ~/.zshrc. Turning
              it off removes the link; the PATH line stays only while a saver still needs it.
            </div>
          </div>
          <Switch
            on={settings.cliTool}
            onChange={(v) => commit({ ...settings, cliTool: v })}
            label="Command line tool"
          />
        </div>
      </div>

      <div className="sect">Updates</div>
      <div className="rows">
        <div className="setrow">
          <div className="smeta">
            <div className="sname">
              {update ? `Version ${update.version} is available` : `Piggy ${APP_VERSION}`}
            </div>
            <div className="sdesc">
              {update
                ? "Piggy will download it, check its signature, and restart."
                : updateNote ?? "Piggy checks only when you ask it to."}
            </div>
          </div>
          {update ? (
            <button className="btn" disabled={busy} onClick={installUpdate}>
              {busy ? "Installing…" : "Install and restart"}
            </button>
          ) : (
            <button className="btn" disabled={checking} onClick={checkUpdate}>
              {checking ? "Checking…" : "Check for updates"}
            </button>
          )}
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
        Piggy {APP_VERSION} · No telemetry, no accounts, and your usage data never leaves your
        Mac. Piggy itself only talks to GitHub. Turning a saver on runs that saver's own
        installer, which downloads from the saver's own home.
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
