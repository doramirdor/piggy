import { useEffect, useState } from "react";
import { api } from "../ipc";
import { useStore } from "../store";
import type { ConfigOption } from "../types";

/** Inline options panel for one saver (expanded under its row). Choice options
 *  render as a segmented control; the chosen value is applied immediately and
 *  re-read from the saver's own config so the UI always shows the truth. */
export function SaverConfigPanel({ saverId }: { saverId: string }) {
  const showError = useStore((s) => s.showError);
  const [options, setOptions] = useState<ConfigOption[] | null>(null);
  const [busyKey, setBusyKey] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .saverConfigGet(saverId)
      .then((opts) => {
        if (!cancelled) setOptions(opts);
      })
      .catch((e) => showError(e));
    return () => {
      cancelled = true;
    };
  }, [saverId, showError]);

  const choose = async (key: string, value: string) => {
    setBusyKey(key);
    try {
      setOptions(await api.saverConfigSet(saverId, key, value));
    } catch (e) {
      showError(e);
    } finally {
      setBusyKey(null);
    }
  };

  if (options === null) {
    return (
      <div className="cfg">
        <div className="desc">Loading options…</div>
      </div>
    );
  }
  if (options.length === 0) {
    return (
      <div className="cfg">
        <div className="desc">No options - this saver works out of the box.</div>
      </div>
    );
  }

  return (
    <div className="cfg">
      {options.map((opt) => (
        <div className="cfg-opt" key={opt.key}>
          <div className="cfg-meta">
            <div className="cfg-label">{opt.label}</div>
            <div className="cfg-desc">
              {opt.choices.find((c) => c.value === opt.current)?.description ?? opt.description}
            </div>
          </div>
          <div className="seg" role="radiogroup" aria-label={opt.label}>
            {opt.choices.map((c) => (
              <button
                key={c.value}
                type="button"
                role="radio"
                aria-checked={opt.current === c.value}
                className={`seg-btn ${opt.current === c.value ? "on" : ""}`}
                disabled={busyKey === opt.key}
                title={c.description}
                onClick={() => opt.current !== c.value && choose(opt.key, c.value)}
              >
                {c.label}
              </button>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
}
