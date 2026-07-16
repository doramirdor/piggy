import { useStore } from "../store";

/** Inline banner for plain-language feedback - never raw JSON. Red "error" alert
 * or a neutral "info" heads-up (e.g. a conflicting saver was auto-disabled). */
export function Banner() {
  const banner = useStore((s) => s.banner);
  const dismiss = useStore((s) => s.dismissBanner);
  if (!banner) return null;
  const info = banner.kind === "info";
  return (
    <div className={`banner ${info ? "info" : ""}`} role={info ? "status" : "alert"}>
      <div style={{ flex: 1, minWidth: 0 }}>
        {banner.title && <div className="btitle">{banner.title}</div>}
        <div className="bbody">{banner.body}</div>
      </div>
      <button className="bclose" onClick={dismiss} aria-label="Dismiss">
        ×
      </button>
    </div>
  );
}
