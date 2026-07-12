# Releasing Piggy

Manual steps a maintainer runs; nothing here blocks local development.

## One-time setup

1. **Apple signing** (required for notarized .dmg):
   - Apple Developer ID Application certificate in the login keychain.
   - `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` (app-specific), `APPLE_TEAM_ID`
     env vars for `tauri build` notarization.
2. **Tauri updater keys**:
   - `npx @tauri-apps/cli signer generate -w ~/.tauri/piggy.key`
   - Public key goes into `app/src-tauri/tauri.conf.json` → `plugins.updater.pubkey`;
     private key stays out of the repo (CI secret `TAURI_SIGNING_PRIVATE_KEY`).
3. **GitHub repo** — create it, then update:
   - `installer/package.json` → `piggy.repo` (currently the `piggy-app/piggy` placeholder)
   - `registry` refresh URL in `crates/piggy-core/src/registry.rs` once the repo exists.

## Each release

1. Bump versions: `app/src-tauri/tauri.conf.json`, workspace `Cargo.toml` crates,
   `installer/package.json`.
2. `cargo test && cargo clippy --all-targets -- -D warnings` — green.
3. `cd app && npm run build && npx vitest run` — green.
4. `npx @tauri-apps/cli build` (universal: run per-arch or use `--target universal-apple-darwin`).
5. Create GitHub release `vX.Y.Z`; upload the `.dmg` + `checksums.txt`
   (`shasum -a 256 *.dmg > checksums.txt`) — the npx installer verifies against this file.
6. `cd installer && npm publish` (only when `piggy.repo`/version metadata changed).
7. Registry updates (new savers, version pins) ship independently by editing
   `registry/catalog.json` on main — the app refreshes it from GitHub raw.

## Principles reminders for releases

- Never bundle optimizer code in the .dmg — Piggy downloads official artifacts at toggle time.
- The release notes must keep measured/claimed language discipline.
