import { useEffect, useState } from "react";
import { api, onAdvisorProgress } from "../ipc";
import { useStore } from "../store";
import { Switch } from "../components/Switch";
import type { AdvisorProgress, AdvisorStatus } from "../types";

/** Sizes here are gigabytes and the difference between 2.4 and 2.5 never changes
 *  a decision, so one decimal is the honest precision. */
function gb(bytes: number): string {
  return `${(bytes / 1_000_000_000).toFixed(1)} GB`;
}

/**
 * The opt-in local advisor.
 *
 * The copy in here does a specific job: make clear that this adds *wording*, not
 * numbers. Piggy's findings are arithmetic on observed tokens and stay exactly
 * as accurate whether this is on or off. If a user comes away thinking the model
 * computed something, the feature has done damage rather than good.
 */
export function AdvisorSettings() {
  const showError = useStore((s) => s.showError);
  const [status, setStatus] = useState<AdvisorStatus | null>(null);
  const [progress, setProgress] = useState<AdvisorProgress | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    api.advisorStatus().then(setStatus).catch((e) => showError(e));
  }, [showError]);

  useEffect(() => {
    let stop: (() => void) | undefined;
    let live = true;
    void onAdvisorProgress((p) => {
      setProgress(p);
      // A finished transfer flips `downloaded`, so the panel has to re-read
      // rather than infer it from the last event.
      if (p.done) api.advisorStatus().then(setStatus).catch(() => {});
    }).then((un) => {
      if (live) stop = un;
      else un();
    });
    return () => {
      live = false;
      stop?.();
    };
  }, []);

  if (!status) return null;

  // Nothing to offer: either no inference is compiled in, or every model in the
  // catalog needs more memory than this machine can spare. Saying which is
  // kinder than a disabled switch with no explanation.
  if (status.state === "unsupported") {
    const tooSmall = status.compiledIn && status.models.length === 0;
    return (
      <div className="setrow">
        <div className="smeta">
          <div className="sname">Explain findings on this Mac</div>
          <div className="sdesc">
            {tooSmall
              ? `Not available on this machine. The smallest model Piggy will run needs more
                 memory than there is to spare${
                   status.hostRamBytes ? ` (this Mac has ${gb(status.hostRamBytes)})` : ""
                 }. Your findings are unaffected: they are measured, not generated.`
              : `This build of Piggy ships without a local model. Your findings are unaffected:
                 they are measured, not generated.`}
          </div>
        </div>
      </div>
    );
  }

  const on = status.state !== "off";
  const selected = status.models.find((m) => m.id === status.selectedId) ?? null;
  const downloading = progress !== null && !progress.done;

  const setModel = async (id: string | null) => {
    setBusy(true);
    try {
      setStatus(await api.advisorSelect(id));
    } catch (e) {
      showError(e);
    } finally {
      setBusy(false);
    }
  };

  const download = async (id: string) => {
    setProgress({ modelId: id, received: 0, total: 0, done: false, error: null });
    try {
      await api.advisorDownload(id);
    } catch (e) {
      setProgress(null);
      showError(e);
    }
  };

  const remove = async (id: string) => {
    setBusy(true);
    try {
      setStatus(await api.advisorRemove(id));
      setProgress(null);
    } catch (e) {
      showError(e);
    } finally {
      setBusy(false);
    }
  };

  const pct =
    progress && progress.total > 0 ? Math.round((progress.received / progress.total) * 100) : 0;

  return (
    <>
      <div className="setrow">
        <div className="smeta">
          <div className="sname">Explain findings on this Mac</div>
          <div className="sdesc">
            Adds a plain-language note under each finding, written by a small model that runs
            entirely on this Mac. It never sees the internet and it never produces a number: every
            figure you see stays Piggy's own arithmetic. Off by default.
          </div>
        </div>
        <Switch
          on={on}
          onChange={(v) => setModel(v ? status.recommendedId : null)}
          label="Explain findings locally"
        />
      </div>

      {on && (
        <div className="setrow advisor-models">
          <div className="smeta">
            <div className="sname">Model</div>
            <div className="sdesc">
              Downloaded once and kept on this Mac. Only models that fit in this machine's spare
              memory are listed.
            </div>
          </div>
          <div className="advisor-list">
            {status.models.map((m) => {
              const active = m.id === status.selectedId;
              const thisDownloading = downloading && progress?.modelId === m.id;
              return (
                <div key={m.id} className={`advisor-card${active ? " on" : ""}`}>
                  <button
                    className="advisor-pick"
                    disabled={busy}
                    onClick={() => setModel(m.id)}
                    aria-pressed={active}
                  >
                    <b>{m.name}</b>
                    <small>{m.blurb}</small>
                    <em>
                      {gb(m.bytes)} download · about {gb(m.peakBytes)} in use
                      {m.downloaded ? " · ready" : ""}
                    </em>
                  </button>

                  {active && !m.downloaded && !thisDownloading && (
                    <button className="advisor-act" onClick={() => download(m.id)}>
                      Download {gb(m.bytes)}
                    </button>
                  )}

                  {thisDownloading && (
                    <div className="advisor-prog">
                      <i className="advisor-track" aria-hidden>
                        <i className="advisor-fill" style={{ width: `${pct}%` }} />
                      </i>
                      <span>{pct}%</span>
                      <button className="advisor-act" onClick={() => api.advisorCancel()}>
                        Cancel
                      </button>
                    </div>
                  )}

                  {m.downloaded && !thisDownloading && (
                    <button className="advisor-act quiet" disabled={busy} onClick={() => remove(m.id)}>
                      Delete
                    </button>
                  )}
                </div>
              );
            })}
          </div>
        </div>
      )}

      {progress?.error && (
        <div className="setrow">
          <div className="smeta">
            <div className="sdesc">{progress.error}</div>
          </div>
        </div>
      )}

      {on && selected && !selected.downloaded && !downloading && (
        <div className="setrow">
          <div className="smeta">
            <div className="sdesc">
              {selected.name} is not downloaded yet, so findings show without notes until it is.
            </div>
          </div>
        </div>
      )}
    </>
  );
}
