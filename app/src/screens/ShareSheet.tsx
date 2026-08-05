import { useEffect, useMemo, useState } from "react";
import { api } from "../ipc";
import { useStore } from "../store";
import {
  CARD_SIZES,
  DEFAULT_SHARE_OPTIONS,
  canvasToPngBase64,
  canvasToPngBlob,
  renderShareCard,
} from "../lib/sharecard-canvas";
import type { ShareCardOptions } from "../lib/sharecard-canvas";
import type { Period, ShareCardData } from "../types";

const INCLUDE: { key: keyof ShareCardOptions["include"]; label: string }[] = [
  { key: "percent", label: "Percent saved" },
  { key: "totals", label: "Totals" },
  { key: "method", label: "Date range & method" },
];

/** The share sheet: a live canvas preview of the card, an options rail, and the
 *  three ways a card leaves the app. Sharing is gated on measured data - when
 *  still measuring, the actions are disabled with a "still measuring" tooltip
 *  (docs/m4-spec.md §"Share card"). */
export function ShareSheet({ period, onClose }: { period: Period; onClose: () => void }) {
  const [data, setData] = useState<ShareCardData | null>(null);
  const [opts, setOpts] = useState<ShareCardOptions>(DEFAULT_SHARE_OPTIONS);
  const [status, setStatus] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const showError = useStore((s) => s.showError);

  useEffect(() => {
    api
      .shareCardData(period)
      .then(setData)
      .catch((e) => showError(e));
  }, [period, showError]);

  const previewSrc = useMemo(() => {
    if (!data) return null;
    return renderShareCard(data, opts).toDataURL("image/png");
  }, [data, opts]);

  const shareable = data?.shareable ?? false;
  const onCount = INCLUDE.filter((i) => opts.include[i.key]).length;

  const toggle = (key: keyof ShareCardOptions["include"]) =>
    setOpts((o) => ({ ...o, include: { ...o.include, [key]: !o.include[key] } }));

  /** Every action renders its own canvas: the preview is a data URL, and the
   *  clipboard and the save command each want a different encoding of it. */
  const withCanvas = async (fn: (c: HTMLCanvasElement) => Promise<string>) => {
    if (!data) return;
    setBusy(true);
    try {
      setStatus(await fn(renderShareCard(data, opts)));
    } catch (e) {
      showError(e);
    } finally {
      setBusy(false);
    }
  };

  const download = () =>
    withCanvas(async (canvas) => {
      const a = document.createElement("a");
      a.href = canvas.toDataURL("image/png");
      a.download = `piggy-savings-${period}.png`;
      a.click();
      return "Downloaded";
    });

  const copy = () =>
    withCanvas(async (canvas) => {
      const blob = await canvasToPngBlob(canvas);
      const clip = navigator.clipboard as Clipboard & { write?: (i: ClipboardItem[]) => Promise<void> };
      if (blob && clip && typeof clip.write === "function" && typeof ClipboardItem !== "undefined") {
        await clip.write([new ClipboardItem({ "image/png": blob })]);
        return "Copied to clipboard";
      }
      // WKWebView without image clipboard support → save-and-reveal fallback.
      const res = await api.saveShareCard(canvasToPngBase64(canvas));
      return `Clipboard unavailable - saved to ${res.path}`;
    });

  const save = () =>
    withCanvas(async (canvas) => {
      const res = await api.saveShareCard(canvasToPngBase64(canvas));
      return `Saved to ${res.path}`;
    });

  const tip = shareable ? "" : "Still measuring - no holdout data yet";

  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <div className="sheet share-sheet" onClick={(e) => e.stopPropagation()}>
        <button className="sheet-close" onClick={onClose} aria-label="Close">
          ✕
        </button>
        <div className="stitle">Share your savings</div>
        <div className="ssub">
          Share a card that shows what changed, how much you saved, and how we measured it.
        </div>

        <div className="share-body">
          {previewSrc ? (
            <img
              className="share-preview"
              style={{ aspectRatio: `${opts.width} / ${opts.height}` }}
              src={previewSrc}
              alt="Piggy savings card preview"
            />
          ) : (
            <div
              className="share-preview"
              style={{ aspectRatio: `${opts.width} / ${opts.height}` }}
            />
          )}

          <div className="share-rail">
            <div className="lp-head">Include in card</div>
            {INCLUDE.map(({ key, label }) => (
              <label key={key} className="share-check">
                <input
                  type="checkbox"
                  checked={opts.include[key]}
                  // Never let the last one off: an empty card is not a card.
                  disabled={opts.include[key] && onCount === 1}
                  onChange={() => toggle(key)}
                />
                {label}
              </label>
            ))}

            <div className="lp-head">Theme</div>
            <div className="share-themes">
              {(["light", "dark"] as const).map((theme) => (
                <button
                  key={theme}
                  className={`share-theme ${theme} ${opts.theme === theme ? "on" : ""}`}
                  aria-pressed={opts.theme === theme}
                  onClick={() => setOpts((o) => ({ ...o, theme }))}
                >
                  <span className="swatch" />
                  {theme === "light" ? "Light" : "Dark"}
                </button>
              ))}
            </div>

            <div className="lp-head">Size</div>
            <select
              className="share-size"
              value={`${opts.width}x${opts.height}`}
              onChange={(e) => {
                const size = CARD_SIZES.find(
                  (s) => `${s.width}x${s.height}` === e.target.value,
                );
                if (size) setOpts((o) => ({ ...o, width: size.width, height: size.height }));
              }}
            >
              {CARD_SIZES.map((s) => (
                <option key={s.label} value={`${s.width}x${s.height}`}>
                  {s.label}
                </option>
              ))}
            </select>
          </div>
        </div>

        {!shareable && (
          <div className="measuring-note">
            Piggy won't share numbers it hasn't measured yet. Run a few more sessions and the
            card will be ready.
          </div>
        )}

        <div className="sactions">
          <div className="tooltip-wrap" title={tip}>
            <button className="btn wide" disabled={!shareable || busy} onClick={download}>
              Download PNG
            </button>
          </div>
          <div className="tooltip-wrap" title={tip}>
            <button className="btn wide" disabled={!shareable || busy} onClick={copy}>
              Copy to clipboard
            </button>
          </div>
          <div className="tooltip-wrap" title={tip}>
            <button className="btn wide solid" disabled={!shareable || busy} onClick={save}>
              Save image
            </button>
          </div>
        </div>

        <div className="measuring-note">
          {status ?? "Numbers only shown when the result is measured."}
        </div>
      </div>
    </div>
  );
}
