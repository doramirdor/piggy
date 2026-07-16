# Piggy measurement design (M3 spec)

The moat is honest numbers. This doc defines exactly how savings are computed and labeled.

## Vocabulary

- **measured** - derived from token counts in session logs. The only thing the dashboard shows.
- **estimated** - anything involving a pricing table or a projection (cost, sweep context-cost).
  Always labeled `(estimated)`. Never blended with measured in one number.

## Why not before/after totals

Raw totals confound task size, model choice, and session length. All comparisons are
**normalized rates**: tokens per assistant turn (deduped assistant messages), per stream
(input / output / cache_create / cache_read), optionally per tool call. Medians preferred over
means (long-tail sessions dominate means); report both internally, display median-based delta.

## Session tagging (ground truth for A/B)

The daemon (file watcher) detects a new session JSONL file and snapshots the currently-enabled
saver set into `session_savers` (session_id, saver_id, enabled) **at file-creation time**.
Sessions that predate Piggy's install are tagged `baseline_pre_install`.

We cannot change a session's config after it starts, so rotation happens **between sessions**:
when a session goes idle (no writes for 10 min) or a new session appears, the rotation
scheduler applies the next planned set so the *next* session picks it up.

## Rotation plan

With master switch on, sessions are assigned (round-robin over a repeating block):

- ~10% **holdout**: all savers off (configurable, `holdout_fraction`).
- For each installed saver X: ~10% **single-off**: everything on except X.
- Remainder: **full-on**.

Rotation only ever toggles Piggy-managed savers. User's own hooks are never touched.
If the user manually flips a toggle, rotation pauses for that saver (respect intent),
and its badge falls back to whatever data exists.

## Savings math

Per saver X:
- ON group: sessions where X enabled. OFF group: sessions where X disabled
  (single-off + holdout + pre-install baseline, flagged separately).
- Delta per stream: `1 - median(rate_on) / median(rate_off)`, displayed as
  `measured 22% less input · 41 sessions`. Confidence interval via bootstrap (1,000 resamples);
  if the 90% CI crosses zero or either group has < 10 sessions → display **"not enough data
  yet · n sessions"** instead of a number. Never show a point estimate without n.
- Overall headline: full-on vs holdout only (not pre-install, unless zero holdouts exist,
  then labeled `vs. history (observational)`). "Your plan lasts N.N× longer" =
  `median_rate(holdout) / median_rate(full_on)` on the plan-metered streams
  (input + output + cache_create at their price weights; cache_read excluded from "spend"
  weighting - labeled estimated because weights come from pricing).
  If weights are involved, the × number is `estimated`; the raw per-stream percentages are
  `measured`. Show the measured percentages first.
- Added latency per saver: median wall-clock gap between consecutive message timestamps in ON
  vs OFF sessions, only if measurable; otherwise omit (never guess).

## Storage

- `session_savers` (session_id, saver_id, enabled, source: rotation|manual|holdout|pre_install)
- `rotation_state` (block position, planned next set, updated_at)
- Attribution queries computed on read; no materialized stats to go stale.

## UI copy rules

- `measured 22% · 41 sessions` - green badge, only when CI excludes zero.
- `not enough data yet · 6 sessions` - neutral badge.
- `claimed 60–90% (author)` - gray, install card only.
- Holdout explainer, one line: "Piggy occasionally runs a session with savers off to measure
  honestly. You can turn this off in Settings (your badges will say 'estimated')."
