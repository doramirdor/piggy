import { useEffect, useMemo, useState } from "react";
import { api } from "../ipc";
import { useStore } from "../store";
import {
  canvasToPngBase64,
  canvasToPngBlob,
  renderShareCard,
} from "../lib/sharecard-canvas";
import type { Period, ShareCardData } from "../types";

/** The share sheet: a live canvas preview of the card + Copy / Save actions.
 *  Sharing is gated on measured data - when still measuring, the buttons are
 *  disabled with a "still measuring" tooltip (docs/m4-spec.md §"Share card"). */
export function ShareSheet({ period, onClose }: { period: Period; onClose: () => void }) {
  const [data, setData] = useState<ShareCardData | null>(null);
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
    return renderShareCard(data).toDataURL("image/png");
  }, [data]);

  const shareable = data?.shareable ?? false;

  const copy = async () => {
    if (!data) return;
    setBusy(true);
    try {
      const canvas = renderShareCard(data);
      const blob = await canvasToPngBlob(canvas);
      const clip = navigator.clipboard as Clipboard & { write?: (i: ClipboardItem[]) => Promise<void> };
      if (blob && clip && typeof clip.write === "function" && typeof ClipboardItem !== "undefined") {
        await clip.write([new ClipboardItem({ "image/png": blob })]);
        setStatus("Copied to clipboard");
      } else {
        // WKWebView without image clipboard support → save-and-reveal fallback.
        const res = await api.saveShareCard(canvasToPngBase64(canvas));
        setStatus(`Clipboard unavailable - saved to ${res.path}`);
      }
    } catch (e) {
      showError(e);
    } finally {
      setBusy(false);
    }
  };

  const save = async () => {
    if (!data) return;
    setBusy(true);
    try {
      const canvas = renderShareCard(data);
      const res = await api.saveShareCard(canvasToPngBase64(canvas));
      setStatus(`Saved to ${res.path}`);
    } catch (e) {
      showError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="sheet-backdrop" onClick={onClose}>
      <div className="sheet" onClick={(e) => e.stopPropagation()}>
        <div className="stitle">Share your savings</div>

        {previewSrc ? (
          <img className="share-preview" src={previewSrc} alt="Piggy savings card preview" />
        ) : (
          <div className="share-preview" />
        )}

        {!shareable && (
          <div className="measuring-note">
            Piggy won't share numbers it hasn't measured yet. Run a few more sessions and the
            card will be ready.
          </div>
        )}

        <div className="sactions">
          <div className="tooltip-wrap" title={shareable ? "" : "Still measuring - no holdout data yet"}>
            <button className="btn wide" disabled={!shareable || busy} onClick={copy}>
              Copy PNG
            </button>
          </div>
          <div className="tooltip-wrap" title={shareable ? "" : "Still measuring - no holdout data yet"}>
            <button className="btn wide green" disabled={!shareable || busy} onClick={save}>
              Save
            </button>
          </div>
        </div>

        {status && <div className="measuring-note">{status}</div>}

        <div className="sactions">
          <button className="btn wide" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}
