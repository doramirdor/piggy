# Releasing Piggy

Manual steps a maintainer runs; nothing here blocks local development.

## One-time setup

1. **Apple signing** (required for notarized .dmg):
   - Apple Developer ID Application certificate in the login keychain.
   - `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` (app-specific), `APPLE_TEAM_ID`
     env vars for `tauri build` notarization.
2. **Tauri updater keys.** Done: a keypair exists at `~/.tauri/piggy.key`, and its public
   half is in `app/src-tauri/tauri.conf.json` → `plugins.updater.pubkey`. The private key is
   **not** in the repo and must not be.
   - Regenerate: `npx @tauri-apps/cli signer generate -w ~/.tauri/piggy.key -f`, then paste
     the new `.pub` into `tauri.conf.json`. **Only safe before the first public release**:
     shipped apps only trust the pubkey they were built with, so rotating the key after
     release strands every installed copy on its current version.
   - The current key has **no passphrase** (CI-friendly). If you want one, regenerate with
     `-p` and set `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` wherever you build.
   - CI secret: `TAURI_SIGNING_PRIVATE_KEY` (the key file's contents or a path to it).
3. **GitHub repo.** Does not exist yet, and the working copy has no git remote. Until it is
   created and a release is published, `npx @amirdor/piggybank` and the README's release links resolve
   to nothing. Create it, then confirm `installer/package.json` → `piggy.repo` matches
   (currently: `doramirdor/piggy`).

## Each release

1. Bump versions, all four: `app/src-tauri/tauri.conf.json`, workspace `Cargo.toml` crates,
   `app/package.json`, `installer/package.json`. Also bump `APP_VERSION` in
   `app/src/screens/Settings.tsx`, the frontend's only copy: it is exported from there and
   imported by `app/src/components/Sidebar.tsx`, which renders it in the sidebar footer.
   Keep it that way: a second hard-coded copy is how the sidebar once shipped a version the
   Settings screen disagreed with.
2. `cargo test && cargo clippy --all-targets -- -D warnings` - green.
3. `cd app && npm run build && npx vitest run` - green. (`npm run build` also stages the
   `piggy` CLI sidecar via `scripts/build-sidecar.mjs`; `tauri build` re-runs it.)
4. Build, with the updater signing key in the environment or the build fails:
   ```sh
   TAURI_SIGNING_PRIVATE_KEY="$HOME/.tauri/piggy.key" \
   TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
   npx @tauri-apps/cli build --target universal-apple-darwin
   ```
   (Or run per-arch. `createUpdaterArtifacts` is on, so every build emits the updater
   `.app.tar.gz` + `.sig` alongside the `.dmg`.)
5. Create GitHub release `vX.Y.Z` and upload:
   - the `.dmg` + `checksums.txt` (`shasum -a 256 *.dmg > checksums.txt`) - the npx
     installer verifies against this file;
   - the updater `.app.tar.gz` + its `.sig`;
   - `latest.json` - the manifest `plugins.updater.endpoints` points at. Its `signature`
     field is the **contents of the `.sig` file**, and its `url` must point at the uploaded
     `.app.tar.gz`:
     ```json
     {
       "version": "X.Y.Z",
       "notes": "…",
       "pub_date": "2026-07-16T00:00:00Z",
       "platforms": {
         "darwin-aarch64": { "signature": "<contents of .sig>", "url": "https://github.com/doramirdor/piggy/releases/download/vX.Y.Z/Piggy.app.tar.gz" },
         "darwin-x86_64":  { "signature": "<contents of .sig>", "url": "https://github.com/doramirdor/piggy/releases/download/vX.Y.Z/Piggy.app.tar.gz" }
       }
     }
     ```
   Skipping `latest.json` doesn't break the app: "Check for updates" just reports that it
   couldn't reach the endpoint.
6. `cd installer && npm publish` (only when `piggy.repo`/version metadata changed).
7. Registry updates (new savers, version pins) currently require a full app release. The
   catalog is embedded at build time (`include_str!` in `crates/piggy-core/src/registry.rs`),
   so editing `registry/catalog.json` on main does **not** reach installed apps. The
   refresh-from-GitHub path is a stub: `Catalog::from_json` exists but no production code
   calls it. Wire it if registry updates need to ship independently of the binary.

## Principles reminders for releases

- Never bundle optimizer code in the .dmg. Piggy installs from each saver's official source at
  toggle time (GitHub release artifacts, PyPI, the Claude plugin marketplace).
- The release notes must keep measured/claimed language discipline.
