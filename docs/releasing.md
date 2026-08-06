# Releasing Piggy

Manual steps a maintainer runs; nothing here blocks local development.

## One-time setup

1. **Apple signing** (required for notarized .dmg):
   - Apple Developer ID Application certificate in the login keychain.
   - `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` (app-specific), `APPLE_TEAM_ID`
     env vars for `tauri build` notarization. CI needs two more, because it has no keychain
     to read the certificate out of: `APPLE_CERTIFICATE` (the base64 `.p12`) and
     `APPLE_CERTIFICATE_PASSWORD`.
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
3. **GitHub repo.** `doramirdor/piggy`, and `installer/package.json` → `piggy.repo` matches.
   Until the first release is published, `npx @amirdor/piggybank` and the README's release
   links resolve to nothing.
4. **CI secrets.** `.github/workflows/release.yml` automates steps 2-5 below on a `vX.Y.Z`
   tag push; the secrets it needs (the six Apple values above, `TAURI_SIGNING_PRIVATE_KEY[_PASSWORD]`)
   are listed in its header comment. Unset Apple secrets degrade to an unsigned build;
   a missing updater key fails the build.

## Each release

The usual path: do step 1, push the `vX.Y.Z` tag, and let CI run steps 2-5 (it gates on
tests and clippy, verifies every version stamp agrees with the tag, and stages a **draft**
release with the .dmg, updater artifacts, `latest.json` and `checksums.txt`). Then do step 6
by hand, whatever built the .dmg: it is the one step that is a person with a Mac, and it is
the only check that the shipped bundle carries a working advisor. Write the notes, publish. Publishing is the go-live moment: the updater
endpoint reads `releases/latest/download`. The manual steps below remain valid as the
fallback and as documentation of what CI does. A `workflow_dispatch` run of the same
workflow builds without releasing, as a dry run for the signing setup.

1. Bump versions, all four: `app/src-tauri/tauri.conf.json`, workspace `Cargo.toml` crates,
   `app/package.json`, `installer/package.json`. Also bump `APP_VERSION` in
   `app/src/screens/Settings.tsx`, the frontend's only copy: it is exported from there and
   imported by `app/src/components/Sidebar.tsx`, which renders it in the sidebar footer.
   Keep it that way: a second hard-coded copy is how the sidebar once shipped a version the
   Settings screen disagreed with.
2. `cargo test --locked && cargo clippy --locked --all-targets -- -D warnings` - green. Then
   `cargo check --locked -p piggy-app --features local-llm`, which is what step 4 actually
   builds: the advisor's code is behind a feature flag, so nothing above compiles a line of
   it, and a break in there surfaces at the end of a long bundle build otherwise. CI runs all
   three (`.github/workflows/release.yml`), with `--locked` so a release cannot quietly
   resolve a different dependency than the one that was tested.
3. `cd app && npm run build && npx vitest run` - green. (`npm run build` also stages the
   `piggy` CLI sidecar via `scripts/build-sidecar.mjs`, and runs `tsc --noEmit`;
   `tauri build` re-runs the sidecar step.)
4. Build, with the updater signing key in the environment or the build fails:
   ```sh
   TAURI_SIGNING_PRIVATE_KEY="$HOME/.tauri/piggy.key" \
   TAURI_SIGNING_PRIVATE_KEY_PASSWORD="" \
   npx @tauri-apps/cli build --target universal-apple-darwin --features local-llm
   ```
   (Or run per-arch. `createUpdaterArtifacts` is on, so every build emits the updater
   `.app.tar.gz` + `.sig` alongside the `.dmg`.) `--features local-llm` compiles the local
   advisor into the bundle, which is what CI passes too; drop it and you ship a build whose
   Settings screen reports the advisor as unsupported. It links llama.cpp, so this build
   needs cmake and a C++ toolchain and takes noticeably longer than a plain one.
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
         "darwin-aarch64": { "signature": "<contents of .sig>", "url": "https://github.com/doramirdor/piggy/releases/download/vX.Y.Z/Piggy_universal.app.tar.gz" },
         "darwin-x86_64":  { "signature": "<contents of .sig>", "url": "https://github.com/doramirdor/piggy/releases/download/vX.Y.Z/Piggy_universal.app.tar.gz" }
       }
     }
     ```
     CI note: tauri-action names the universal updater artifact
     `Piggy_universal.app.tar.gz`, and the `notes` field is frozen at build
     time from the workflow's neutral `releaseBody`; the human-written notes
     go on the release body before publishing and do not reach `latest.json`.
   Skipping `latest.json` doesn't break the app: "Check for updates" just reports that it
   couldn't reach the endpoint.
6. **Smoke-test the .dmg on a machine that has never run Piggy.** This is the half of M5's
   first acceptance criterion no test can reach, and it is here rather than approximated in
   one. A build test can prove `--features local-llm` was passed (it does:
   `app/src-tauri/src/advisor.rs`, `the_shipped_bundle_compiles_the_advisor_in_and_the_test_path_does_not`).
   It cannot prove the thing that matters, which is that a person who has never seen Piggy
   can download the .dmg and get a working advisor. Walk it:
   - Install from the `.dmg` (or `npx @amirdor/piggybank`) on a clean Mac, or a fresh user
     account, and open the app.
   - Settings shows the **local advisor** section, and it does **not** say the advisor is
     unsupported on this build. That sentence means the feature flag was dropped, which is
     the one failure that no test on the release path catches.
   - Pick a model and download it. Watch the progress, then cancel a download and restart it:
     it resumes rather than starting the multi-gigabyte transfer again.
   - Pull the network mid-download. The failure must arrive as a plain sentence, and the app
     must keep working with the advisor off, because everything degrades to the deterministic
     product by design.
   - With no model downloaded, open Spend. The suggestions are there, in the same order, with
     Piggy's own wording, and an oversized CLAUDE.md card offers to turn the advisor on. With
     a model downloaded, the same card either shows a diff or says plainly that the model
     could not produce a rewrite worth applying. It must never tell someone who already has
     the advisor on to turn it on.
7. `cd installer && npm publish` (only when `piggy.repo`/version metadata changed).
8. Registry updates (new savers, version pins) currently require a full app release. The
   catalog is embedded at build time (`include_str!` in `crates/piggy-core/src/registry.rs`),
   so editing `registry/catalog.json` on main does **not** reach installed apps. The
   refresh-from-GitHub path is a stub: `Catalog::from_json` exists but no production code
   calls it. Wire it if registry updates need to ship independently of the binary.

## Principles reminders for releases

- Never bundle optimizer code in the .dmg. Piggy installs from each saver's official source at
  toggle time (GitHub release artifacts, PyPI, the Claude plugin marketplace).
- The release notes must keep measured/claimed language discipline.
