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

"All savers off" is the intent, not a guarantee. Rotation only controls savers it manages,
so a saver the user has pinned on keeps running through the holdout slot. A holdout with a
pinned saver in it is still real evidence about the savers that *do* rotate, and it is still
used, but it is not the no-savers baseline the headline's "N.N× longer" is a claim about,
so it caps that headline at `estimated`.

The mirror case is a saver the user switched **off** by hand. It is off in the holdout and
off in every full-on session too, so it is a constant rather than a confound: it simply
leaves the comparison, the way an uninstalled saver would. "Full-on" therefore means every
saver the *scheduler* is running is on, not that no saver anywhere is off, and what gets
measured ("everything else on" vs "nothing on") is exactly the setup that user runs. The
earlier, stricter reading classified every one of their sessions as a single-off slot and
left the headline reading "measuring" forever, at any session count.

Rotation only ever toggles Piggy-managed savers. User's own hooks are never touched.
If the user manually flips a toggle, rotation pauses for that saver (respect intent).
Piggy is then no longer randomizing it, so no *new* measured evidence accrues for it:
post-toggle sessions are observational and are ignored for a measured claim. The badge
stays `measured` only on the strength of the randomized era that came before the toggle,
and falls to `estimated` once that era is too thin to stand on its own (see below). That
is the honest trade for respecting intent, and it is symmetric with the pre-install
baseline.

## Savings math

Per saver X:
- ON group: sessions where X enabled. OFF group: sessions where X disabled. **Both** sides
  additionally require every *other* saver to be on, so the only thing differing between
  them is X. Rotation turns X off in two different slots and they are not the same
  treatment: the single-off slot (X off, everything else running) isolates X, while the
  holdout (X off and everything else off too) is the whole-bundle comparison the headline
  makes. Pooling them put X's number up against a mix of both, so the other savers'
  savings landed on X: at the default holdout fraction that mix is 50/50 for every user by
  construction, and a saver whose true effect was 50% reported 71% once 30 holdouts
  existed. Note the consequence for the pre-install baseline: with 2+ savers installed it
  is "nothing on", so it cannot isolate X either, and X stays `measuring` until real
  single-off data exists rather than showing a figure that credits X with everything.
  With exactly one saver installed there are no others, holdout and single-off are the
  same state, and all of this collapses to "X on vs X off" as before.
- **Both** groups are split by source, not just the OFF one. `rotation` / `holdout` rows
  are randomized and can back a `measured` badge; `manual` / `pre_install` rows are
  observational and cap that side at `estimated`. Each side prefers its randomized rows
  when they reach the 10-session bar, and pools in the observational ones only when they
  do not. The weaker side governs the badge: a randomized OFF group cannot launder a
  manual-on ON group into a measured claim. Randomization is a property of the contrast,
  so "recent manual-on era vs older randomized-off era" is observational, and any drift
  between the eras would otherwise land on the saver.
- Delta per stream: `1 - median(rate_on) / median(rate_off)`, displayed as
  `measured 22% less input · 41 sessions`. Confidence interval via bootstrap (1,000 resamples);
  if the 90% CI crosses zero or either group has < 10 sessions → display **"not enough data
  yet · n sessions"** instead of a number. Never show a point estimate without n.
  An **empty** ON group has no delta at all, not a 100% one: `median` of nothing is 0, so the
  formula would otherwise read `1 - 0/median_off = 1.0` and claim a perfect saving from no
  data. A non-empty ON group that medians to zero is different, and is a real 100% reduction.
- Overall headline: full-on vs holdout only (not pre-install, unless zero holdouts exist,
  then labeled `vs. history (observational)`). The full-on side is scoped to the saver set
  from your **most recent** full-on session, i.e. the setup you are running now. A session
  records no saver set of its own, so without that scoping the ON group pools every era the
  setup has ever been in: install a saver, uninstall one, hand-toggle one, and "everything
  on" quietly means something different on either side of that moment. The median then
  tracks the era mix rather than the savers. Recency rather than majority, because
  "your plan lasts N.N× longer" is a claim about the setup you have now: a larger pile of
  sessions from a configuration you abandoned must not outvote it. Changing your saver set
  therefore restarts the full-on count, and the headline honestly reads "measuring" until
  10 sessions of the new setup land. The baseline is deliberately *not* scoped that way,
  and that is a trade, not a symmetry: every holdout is "nothing on" so the treatment does
  match across eras, but era drift still rides along, and randomization only balances drift
  *within* an era. Holdouts are ~1 in 10 sessions, so scoping them to one era would put the
  10-session bar out of reach for most people and the headline would never light up at all.
  Sample viability wins on that arm, and the cost is stated rather than hidden.
  The same both-sides rule applies here: a
  full-on session counts as randomized only if **every** saver in it was on because the
  scheduler said so. A session where any saver is pinned on by hand is an observational
  full-on, so a holdout baseline alone does not earn `measured` and the headline is
  labelled `estimated` instead. Both `piggy report` and the GUI headline check this, not
  just the baseline kind. "Your plan lasts N.N× longer" =
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
