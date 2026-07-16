# 🐷 Piggy

**Your Claude plan, but longer.**

Piggy is a free, open-source macOS menu bar app for Claude Code users who keep hitting usage
limits. It installs the best community token savers with one toggle - no terminal, no config
files - and then does something nobody else does: **it measures whether they actually work.**

> *The App Store - and the referee - for Claude Code token savers.*

## How it works

1. **Flip the switch.** Piggy installs a curated set of token savers in the right order,
   backing up your Claude settings first. Everything is reversible with one click.
2. **Keep coding like always.** Piggy reads Claude Code's own session logs to count every
   token - input, output, cache - straight from the source.
3. **See honest numbers.** A small share of sessions run with savers off (a *holdout*), so
   Piggy can show you *measured* savings, not marketing claims:
   `−22% measured` beats `60–90% claimed` every day.

## Honesty rules

- **measured** numbers come from your real session logs, compared against holdout sessions.
- **estimated** numbers involve a pricing table or a projection, and are always labeled.
- The two are never blended. If there isn't enough data, Piggy says *"measuring"*. It never
  shows a number it can't back.
- Saver authors' own claims appear only on install cards, labeled *claimed*.

## Privacy

No telemetry. No accounts. Your usage data never leaves your Mac. The only network calls
Piggy makes are to GitHub: refreshing the saver catalog, downloading official saver releases
(checksum-verified), and listing newly discovered tools.

## Install

```
npx piggybank        # downloads and opens the notarized app
```

or grab the latest `.dmg` from [Releases](../../releases).

Command-line fans get a standalone CLI too:

```
piggy stats          # today / week / month token totals, per project and model
piggy doctor         # checks your setup and Piggy's own health
```

## Run Claude through Piggy

Most savers work in every session automatically. The deepest one, Headroom, is scoped on
purpose: it compresses only the sessions you start with `piggy-claude`, a launcher Piggy
adds when you turn Headroom on.

```
piggy-claude    # Claude Code with deep compression
claude          # plain Claude Code, untouched
```

Use `piggy-claude` wherever you'd normally run `claude`. If anything ever misbehaves, plain
`claude` keeps working exactly as before - nothing about your normal setup changes.

## For saver authors

Piggy never forks or vendors your code. It downloads your official release artifacts, pins
known-good versions, and verifies checksums. Want your tool listed? Open a PR against
`registry/catalog.json`. Honest measurement is applied equally to everyone.

## Status

All four milestones built and tested: ✅ measurement core · ✅ install engine · ✅ holdout
measurement · ✅ menu bar app (89 Rust + 21 UI tests). Not yet released - first .dmg needs
the signing/notarization steps in [docs/releasing.md](docs/releasing.md). See [DESIGN.md](DESIGN.md).

## License

MIT
