#!/usr/bin/env node
/**
 * Stage the `piggy` CLI as a Tauri sidecar.
 *
 * Tauri's `bundle.externalBin` resolves `binaries/piggy` to
 * `binaries/piggy-<target-triple>` at build time and copies the match into
 * `Piggy.app/Contents/MacOS/piggy`, next to the app's own executable. This
 * script produces that suffixed file.
 *
 * The triple comes from `TAURI_ENV_TARGET_TRIPLE` (set by Tauri for
 * `beforeDevCommand`/`beforeBuildCommand`), else `--target`, else the host.
 * `universal-apple-darwin` is special: Cargo cannot build it directly, so both
 * arches are built and `lipo`-ed together, which is what a universal .dmg needs.
 *
 * Output is git-ignored: it is a build artifact, rebuilt on every dev/build run.
 */
import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const appDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = resolve(appDir, "..");
const outDir = join(appDir, "src-tauri", "binaries");

const UNIVERSAL = "universal-apple-darwin";
const UNIVERSAL_MEMBERS = ["aarch64-apple-darwin", "x86_64-apple-darwin"];

function run(cmd, args, opts = {}) {
  return execFileSync(cmd, args, { cwd: repoRoot, encoding: "utf8", ...opts });
}

function hostTriple() {
  const match = run("rustc", ["-vV"]).match(/^host:\s*(\S+)$/m);
  if (!match) throw new Error("could not read the host triple from `rustc -vV`");
  return match[1];
}

function requestedTriple() {
  const flag = process.argv.indexOf("--target");
  if (flag !== -1 && process.argv[flag + 1]) return process.argv[flag + 1];
  return process.env.TAURI_ENV_TARGET_TRIPLE || hostTriple();
}

/** Build piggy-cli for one triple and return the path to the binary. */
function buildOne(triple, host) {
  if (triple !== host) {
    // Cross-compiling (e.g. the x86_64 half of a universal build on Apple
    // silicon) needs the std lib for that target. No-op once installed.
    run("rustup", ["target", "add", triple], { stdio: "inherit" });
  }
  run("cargo", ["build", "--release", "-p", "piggy-cli", "--target", triple], {
    stdio: "inherit",
  });
  return join(repoRoot, "target", triple, "release", "piggy");
}

const triple = requestedTriple();
const host = hostTriple();
mkdirSync(outDir, { recursive: true });
const dest = join(outDir, `piggy-${triple}`);

if (triple === UNIVERSAL) {
  const halves = UNIVERSAL_MEMBERS.map((t) => buildOne(t, host));
  run("lipo", ["-create", "-output", dest, ...halves], { stdio: "inherit" });
} else {
  copyFileSync(buildOne(triple, host), dest);
}
chmodSync(dest, 0o755);

console.log(`sidecar staged: ${dest}`);
