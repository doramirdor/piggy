import { useEffect, useState } from "react";
import { api } from "../ipc";
import { useStore } from "../store";
import type { DiscoverDto } from "../types";

export function Discover() {
  const [data, setData] = useState<DiscoverDto | null>(null);
  const [refreshing, setRefreshing] = useState(false);
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

  const refresh = async () => {
    setRefreshing(true);
    try {
      setData(await api.refreshDiscovered());
    } catch (e) {
      showError(e);
    } finally {
      setRefreshing(false);
    }
  };

  return (
    <>
      <div className="head">
        <div>
          <h1>Discover</h1>
          <div className="sub">
            Candidates Piggy spotted on GitHub. Piggy has not vetted or measured any of them, and
            nothing here installs. Read them on GitHub and judge for yourself.
          </div>
        </div>
        <button className="btn" disabled={refreshing} onClick={refresh}>
          {refreshing ? "Checking…" : "Check now"}
        </button>
      </div>

      <div className="disc-grid">
        {data?.feed.map((f) => (
          <div className="disc" key={f.name}>
            <div className="dtop">
              <span className="dname">{f.name}</span>
              {f.stars != null && (
                <span className="stars">★ {f.stars.toLocaleString("en-US")}</span>
              )}
            </div>
            <div className="ddesc">{f.description}</div>
            {/* The backend always sends authorClaims: null today, so this never
                renders. Kept because the field is still in the DTO and any claim
                shown here would be the author's own, never Piggy's measurement. */}
            {f.authorClaims && <span className="claim">{f.authorClaims}</span>}
            <div className="dfoot">
              {f.repoUrl && (
                <button className="ghlink" onClick={() => open(f.repoUrl)}>
                  View on GitHub →
                </button>
              )}
            </div>
          </div>
        ))}
      </div>

      {data && data.listedOnly.length > 0 && <div className="sect">Listed for transparency</div>}
      <div className="disc-grid">
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
                <button className="ghlink" onClick={() => open(e.repoUrl)}>
                  View on GitHub →
                </button>
              )}
              <span className="lic">{e.license}</span>
            </div>
          </div>
        ))}
      </div>

      {data && data.feed.length === 0 && (
        <div className="foot-note">
          Piggy checks GitHub for new savers about once a day. None to show right now.
        </div>
      )}
    </>
  );
}
