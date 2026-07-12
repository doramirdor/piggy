# piggybank

The `npx` installer for **Piggy**, a macOS menu bar app. Zero dependencies —
Node stdlib only (`https`, `crypto`, `fs`, `os`, `child_process`, `readline`).

## Usage

Install the latest release:

```sh
npx piggybank
```

This will:

1. Confirm you're on macOS (friendly message + exit otherwise).
2. Detect your Mac's architecture (`arm64` or `x64`).
3. Ask the GitHub API for the configured release (see [Configuration](#configuration)).
4. Find the `.dmg` asset that matches your architecture.
5. Download it to a temp directory with a progress indicator.
6. Verify its `sha256` against a `checksums.txt` asset published alongside it.
   If either the checksums file or a matching hash is missing, the installer
   **hard-fails** rather than installing an unverified binary.
7. Mount the `.dmg` with `hdiutil attach`.
8. Ask **[y/N]** whether to copy `Piggy.app` straight into `/Applications`.
   - **Yes** → copies it in, then detaches the disk image. Done.
   - **No** → opens the mounted volume in Finder so you can drag it in
     yourself, waits for you to press Enter, then detaches.

Skip the copy prompt (assume yes) — handy for CI or scripted setups:

```sh
npx piggybank --yes
```

Uninstall:

```sh
npx piggybank --uninstall
```

Prompts **[y/N]** before removing `/Applications/Piggy.app` (or pass `--yes`
to skip the prompt). It deliberately leaves `~/.piggy` (your saved data)
alone and prints the command to remove it yourself:

```sh
rm -rf ~/.piggy
```

See all flags:

```sh
npx piggybank --help
```

## Configuration

The installer reads its target repo and version from **this package's own
`package.json`**, under the `piggy` field:

```json
{
  "piggy": {
    "repo": "piggy-app/piggy",
    "version": "latest"
  }
}
```

- `piggy.repo` — the GitHub `<owner>/<name>` to fetch releases from.
  `piggy-app/piggy` is a **placeholder** until the real Piggy repo exists.
- `piggy.version` — a release tag (e.g. `"v1.2.0"`) or `"latest"`.

### Updating this at release time

When the real Piggy GitHub repo is created (and again any time the intended
install target changes), update `installer/package.json`:

1. Set `piggy.repo` to the real `<owner>/<name>`.
2. Leave `piggy.version` as `"latest"` for normal releases — the installer
   always resolves whatever GitHub currently marks as the latest release via
   `GET /repos/<repo>/releases/latest`. Only pin `piggy.version` to a
   specific tag if you deliberately want `npx piggybank` to install an
   older/specific version.
3. Bump the installer's own `version` field (semver for the `piggybank` npm
   package itself — independent of the Piggy app version) and `npm publish`
   from `installer/`.

The release side of the contract (owned by whatever builds and publishes
Piggy):

- Each GitHub release must attach `.dmg` assets whose filenames contain an
  arch tag the installer recognizes: `arm64`/`aarch64` for Apple Silicon,
  `x64`/`x86_64`/`amd64` for Intel (e.g. `Piggy-1.2.0-arm64.dmg`). A release
  with a single untagged `.dmg` also works (treated as universal/either-arch).
- Each release must also attach a `checksums.txt` asset listing the
  `sha256` of every `.dmg`, one per line, in standard `sha256sum` format:

  ```
  <sha256-hex>  Piggy-1.2.0-arm64.dmg
  <sha256-hex>  Piggy-1.2.0-x64.dmg
  ```

  Typically generated with `shasum -a 256 *.dmg > checksums.txt`.

Until the repo has a first release, `npx piggybank` prints:

> Piggy hasn't shipped its first release yet — watch
> https://github.com/\<repo\>/releases

and exits cleanly (this is GitHub's normal `404` response from
`/releases/latest` on a repo with no published releases yet — not an error
in the installer).

## Development

```sh
cd installer
npm test          # runs test/*.test.js via node --test — no network required
node cli.js --help
```

### Design notes

- `lib.js` holds every pure function: release-URL building, config
  resolution, arch mapping, asset picking (`.dmg` + `checksums.txt`),
  checksum-file parsing, checksum verification, the "no releases yet"
  message, and `hdiutil attach` output parsing. None of it touches the
  network, the filesystem, or a child process — that's what makes it
  testable offline with fixture JSON in `test/fixtures/`.
- `cli.js` is the thin, imperative shell around `lib.js`: HTTP calls,
  streaming download + progress bar, `hdiutil`/`open` invocations, the
  readline `[y/N]` prompts, and ANSI-colored output. It has no exported
  surface to unit test directly — it's exercised by hand (`node cli.js`)
  and via the acceptance path in the M4 spec (`npx piggybank` → app opens).

### Tests

```sh
npm test
```

Runs `test/lib.test.js` against fixtures in `test/fixtures/` (sample GitHub
release JSON, a `checksums.txt`, and sample `hdiutil attach` output) —
30 cases covering arch mapping, config resolution, release URL building,
`.dmg`/checksum asset picking (including single-dmg and no-dmg releases),
checksum-file parsing (including the `sha256sum -b` `*filename` form),
checksum verification, the no-release message, byte formatting, and
`hdiutil` mount-point parsing. No network access required.
