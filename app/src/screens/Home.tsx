import { useEffect, useState } from "react";
import { useStore } from "../store";
import { api } from "../ipc";
import { Switch } from "../components/Switch";
import { BadgeChip } from "../components/Badge";
import { StreamBars } from "../components/StreamBars";
import { saverIcon } from "../components/saverMeta";
import { formatTokens } from "../lib/format";
import { SweepSheet } from "./SweepSheet";
import type { SaverRow, SweepReport } from "../types";

function WarnDot({ text }: { text: string | null }) {
  return (
    <button
      type="button"
      className="warn-dot"
      title={text ?? "Changes how Claude behaves"}
      aria-label={text ?? "Changes how Claude behaves"}
    />
  );
}

function SaverRowItem({ saver }: { saver: SaverRow }) {
  const busy = useStore((s) => s.busySavers.includes(saver.id));
  const toggle = useStore((s) => s.toggleSaver);
  const { glyph, tint } = saverIcon(saver.id);
  const name = saver.plainLabel ?? saver.name;
  return (
    <div className="row">
      <div className="ic" style={{ background: tint }}>
        {glyph}
      </div>
      <div className="meta">
        <div className="name">
          {name}
          {saver.behaviorChanging && <WarnDot text={saver.warning} />}
        </div>
        <div className="desc">{saver.description}</div>
      </div>
      <BadgeChip badge={saver.badge} />
      <Switch
        sm
        on={saver.enabled}
        busy={busy}
        onChange={(next) => toggle(saver.id, next)}
        label={`Turn ${name} ${saver.enabled ? "off" : "on"}`}
      />
    </div>
  );
}

function HeadlineStrip() {
  const stats = useStore((s) => s.stats);
  if (!stats) return null;
  const h = stats.headline;
  const measured = h.label === "measured" && h.value != null;
  const estimated = h.label === "estimated" && h.value != null;
  return (
    <div className="dash">
      <div className="headline">Your Claude plan lasts</div>
      {measured ? (
        <>
          <div className="big">
            <em>{h.value!.toFixed(1)}×</em> longer
          </div>
          <div className="sub">
            measured against {h.nHoldout} holdout sessions · {stats.periodLabel.toLowerCase()}
          </div>
        </>
      ) : estimated ? (
        <>
          <div className="big">
            <em>~{h.value!.toFixed(1)}×</em> longer
          </div>
          <div className="sub">estimated vs your history · holdout measurement in progress</div>
        </>
      ) : (
        <>
          <div className="big" style={{ fontSize: 18, color: "var(--text-2)" }}>
            measuring…
          </div>
          <div className="sub">{h.nHoldout} of 10 holdout sessions so far</div>
        </>
      )}
      <StreamBars streams={stats.streams} />
    </div>
  );
}

export function Home() {
  const savers = useStore((s) => s.savers);
  const masterOn = savers?.masterOn ?? false;
  const masterBusy = useStore((s) => s.masterBusy);
  const toggleMaster = useStore((s) => s.toggleMaster);
  const showError = useStore((s) => s.showError);

  const [sweep, setSweep] = useState<SweepReport | null>(null);
  const [sweepOpen, setSweepOpen] = useState(false);

  useEffect(() => {
    api
      .sweepReport()
      .then(setSweep)
      .catch((e) => showError(e));
  }, [showError, sweepOpen]);

  const recommended = sweep?.items.filter((i) => i.recommendDisable) ?? [];

  return (
    <div className="scroll">
      <div className="master">
        <div className="txt">
          <div className="t1">Save everything</div>
          <div className="t2">Piggy's curated set, safely ordered · reversible anytime</div>
        </div>
        <Switch on={masterOn} busy={masterBusy} onChange={toggleMaster} label="Save everything" />
      </div>

      <div className="sect">Savers</div>
      <div className="rows">
        {savers?.savers.map((s) => (
          <SaverRowItem key={s.id} saver={s} />
        ))}
      </div>

      <HeadlineStrip />

      {recommended.length > 0 && sweep && (
        <div className="hint">
          <div className="t">
            <b>
              {recommended.length} add-on{recommended.length === 1 ? "" : "s"} you never use
            </b>{" "}
            {recommended.length === 1 ? "is" : "are"} costing ~
            {formatTokens(sweep.estRecoverableTokens)} tokens per request. <small>estimated</small>
          </div>
          <button className="btn" onClick={() => setSweepOpen(true)}>
            Review
          </button>
        </div>
      )}

      {sweepOpen && <SweepSheet onClose={() => setSweepOpen(false)} />}
    </div>
  );
}
