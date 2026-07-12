import { useEffect, useState } from "react";
import { api } from "../ipc";
import { useStore } from "../store";
import type { DiscoverDto } from "../types";

export function Discover() {
  const [data, setData] = useState<DiscoverDto | null>(null);
  const showError = useStore((s) => s.showError);

  useEffect(() => {
    api
      .discoveredList()
      .then(setData)
      .catch((e) => showError(e));
  }, [showError]);

  const open = (url: string | null) => {
    if (url) void api.openExternal(url);
  };

  return (
    <div className="scroll page">
      <div className="page-title">Discover</div>
      <div className="foot-note" style={{ textAlign: "left", padding: "2px 18px 4px" }}>
        Savers Piggy has found in the wild. Nothing here is installable yet — claims are the
        author's own, never Piggy's measurements.
      </div>

      {data?.feed.map((f) => (
        <div className="disc" key={f.name}>
          <div className="dtop">
            <span className="dname">{f.name}</span>
            {f.stars != null && <span className="stars">★ {f.stars.toLocaleString("en-US")}</span>}
          </div>
          <div className="ddesc">{f.description}</div>
          {f.authorClaims && <span className="claim">{f.authorClaims}</span>}
          <div className="dfoot">
            {f.repoUrl && (
              <button className="ghlink" style={{ background: "none", border: 0 }} onClick={() => open(f.repoUrl)}>
                View on GitHub →
              </button>
            )}
          </div>
        </div>
      ))}

      {data && data.listedOnly.length > 0 && <div className="sect">Listed for transparency</div>}
      {data?.listedOnly.map((e) => (
        <div className="disc" key={e.id}>
          <div className="dtop">
            <span className="dname">{e.name}</span>
          </div>
          <div className="ddesc">{e.description}</div>
          {e.claimedSavings && <span className="claim">author claims {e.claimedSavings}</span>}
          {e.exclusionReason ? (
            <div className="excl">
              <b>Why Piggy won't install it: </b>
              {e.exclusionReason}
            </div>
          ) : (
            <div className="excl" style={{ background: "rgba(127,127,127,0.14)", color: "var(--text-2)" }}>
              {e.note}
            </div>
          )}
          {e.licenseNote && (
            <div className="ddesc" style={{ marginTop: 6 }}>
              <b>{e.license}: </b>
              {e.licenseNote}
            </div>
          )}
          <div className="dfoot">
            {e.repoUrl && (
              <button className="ghlink" style={{ background: "none", border: 0 }} onClick={() => open(e.repoUrl)}>
                View on GitHub →
              </button>
            )}
            <span className="lic">{e.license}</span>
          </div>
        </div>
      ))}

      {data && data.feed.length === 0 && (
        <div className="foot-note">
          Piggy checks GitHub for new savers about once a day. None to show right now.
        </div>
      )}
    </div>
  );
}
