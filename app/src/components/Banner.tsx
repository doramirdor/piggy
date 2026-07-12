import { useStore } from "../store";

/** The red inline error banner. Renders plain-language text — never raw JSON. */
export function Banner() {
  const banner = useStore((s) => s.banner);
  const dismiss = useStore((s) => s.dismissBanner);
  if (!banner) return null;
  return (
    <div className="banner" role="alert">
      <div style={{ flex: 1, minWidth: 0 }}>
        <div className="btitle">{banner.title}</div>
        <div className="bbody">{banner.body}</div>
      </div>
      <button className="bclose" onClick={dismiss} aria-label="Dismiss">
        ×
      </button>
    </div>
  );
}
