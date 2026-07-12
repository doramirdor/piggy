'use strict';

/**
 * Pure, network-free logic for the piggybank installer.
 *
 * Everything in this file takes plain data in and returns plain data out —
 * no fs, no https, no child_process. That's what lets test/lib.test.js
 * exercise the release-resolution / asset-pick / checksum-verify logic
 * with fixture JSON and no network access.
 */

const crypto = require('crypto');

// Alternate spellings vendors sometimes use for each arch in asset filenames.
const ARCH_ALIASES = {
  arm64: ['arm64', 'aarch64'],
  x64: ['x64', 'x86_64', 'amd64'],
};

/**
 * Map Node's os.arch() to the arch tag we expect in release asset filenames.
 * Throws for anything Piggy doesn't ship (Macs are arm64 or x64 only).
 */
function mapNodeArchToAssetArch(nodeArch) {
  if (nodeArch === 'arm64') return 'arm64';
  if (nodeArch === 'x64') return 'x64';
  throw new Error(
    `Unsupported architecture "${nodeArch}". Piggy ships for Apple Silicon (arm64) and Intel (x64) Macs.`
  );
}

/**
 * Resolve installer config from the installed package's own package.json,
 * falling back to sane defaults (used before the real repo exists).
 */
function resolveConfig(pkgJson) {
  const cfg = (pkgJson && pkgJson.piggy) || {};
  return {
    repo: cfg.repo || 'amirdoramir/piggy',
    version: cfg.version || 'latest',
  };
}

/**
 * Build the GitHub REST API URL to fetch a release from.
 */
function buildReleaseApiUrl(repo, version) {
  if (!repo || typeof repo !== 'string' || !repo.includes('/')) {
    throw new Error(`Invalid repo "${repo}" — expected "<owner>/<name>"`);
  }
  if (!version || version === 'latest') {
    return `https://api.github.com/repos/${repo}/releases/latest`;
  }
  return `https://api.github.com/repos/${repo}/releases/tags/${version}`;
}

/**
 * Given a release's `assets` array, find the .dmg for the requested arch.
 * Falls back to the sole .dmg asset if there's exactly one and none of the
 * names carry an arch tag (single-arch release).
 */
function findDmgAsset(assets, assetArch) {
  if (!Array.isArray(assets)) return null;
  const aliases = ARCH_ALIASES[assetArch] || [assetArch];
  const dmgAssets = assets.filter(
    (a) => a && typeof a.name === 'string' && a.name.toLowerCase().endsWith('.dmg')
  );
  const matched = dmgAssets.find((a) =>
    aliases.some((alias) => a.name.toLowerCase().includes(alias))
  );
  if (matched) return matched;
  if (dmgAssets.length === 1) return dmgAssets[0];
  return null;
}

/**
 * Find the checksums asset in a release (checksums.txt / checksum.txt,
 * case-insensitive).
 */
function findChecksumsAsset(assets) {
  if (!Array.isArray(assets)) return null;
  return (
    assets.find((a) => a && typeof a.name === 'string' && /^checksums?\.txt$/i.test(a.name)) ||
    null
  );
}

/**
 * Parse a checksums.txt file (standard `sha256sum` style output:
 * "<hex>  <filename>" or "<hex> *<filename>", comments/blank lines ignored)
 * into a Map<filename, lowercase hex sha256>.
 */
function parseChecksumsFile(content) {
  const map = new Map();
  if (!content) return map;
  const lines = content.split(/\r?\n/);
  for (const rawLine of lines) {
    const line = rawLine.trim();
    if (!line || line.startsWith('#')) continue;
    const match = line.match(/^([a-fA-F0-9]{64})\s+\*?(.+)$/);
    if (match) {
      map.set(match[2].trim(), match[1].toLowerCase());
    }
  }
  return map;
}

/**
 * Look up the expected checksum for a filename, tolerating checksum files
 * that prefix entries with a path (e.g. "dist/Piggy.dmg").
 */
function getExpectedChecksum(checksumMap, filename) {
  if (!checksumMap || !filename) return null;
  if (checksumMap.has(filename)) return checksumMap.get(filename);
  for (const [name, hash] of checksumMap.entries()) {
    if (name === filename || name.endsWith(`/${filename}`)) return hash;
  }
  return null;
}

/**
 * sha256 hex digest of an in-memory buffer/string.
 */
function sha256Hex(bufferOrString) {
  return crypto.createHash('sha256').update(bufferOrString).digest('hex');
}

/**
 * Case-insensitive hex digest comparison.
 */
function hashesMatch(a, b) {
  if (!a || !b) return false;
  return a.toLowerCase() === b.toLowerCase();
}

/**
 * Convenience wrapper: hash a buffer and compare to an expected hex digest.
 */
function verifyChecksum(buffer, expectedHex) {
  if (!expectedHex) return false;
  return hashesMatch(sha256Hex(buffer), expectedHex);
}

/**
 * The friendly message shown when the configured repo has no releases yet.
 */
function buildNoReleaseMessage(repo) {
  return `Piggy hasn't shipped its first release yet — watch https://github.com/${repo}/releases`;
}

/**
 * GitHub's API returns 404 for /releases/latest when a repo has zero
 * published releases — that's how we detect the "no releases yet" case.
 */
function isNotFoundStatus(statusCode) {
  return statusCode === 404;
}

/**
 * Human-readable byte size, e.g. 52428800 -> "50.0 MB".
 */
function formatBytes(bytes) {
  if (!bytes || bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB'];
  let n = bytes;
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i += 1;
  }
  return `${n.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

/**
 * Parse the mount point out of `hdiutil attach` text output. Real output
 * looks like tab-separated columns, the last of which is the mount path
 * for the HFS/APFS partition row, e.g.:
 *   /dev/disk4          GUID_partition_scheme
 *   /dev/disk4s1        Apple_HFS               /Volumes/Piggy
 */
function parseHdiutilAttachOutput(output) {
  if (!output) return null;
  const lines = output.split(/\r?\n/);
  let found = null;
  for (const line of lines) {
    const match = line.match(/(\/Volumes\/[^\t]+?)\s*$/);
    if (match) {
      found = match[1].trim();
    }
  }
  return found;
}

module.exports = {
  ARCH_ALIASES,
  mapNodeArchToAssetArch,
  resolveConfig,
  buildReleaseApiUrl,
  findDmgAsset,
  findChecksumsAsset,
  parseChecksumsFile,
  getExpectedChecksum,
  sha256Hex,
  hashesMatch,
  verifyChecksum,
  buildNoReleaseMessage,
  isNotFoundStatus,
  formatBytes,
  parseHdiutilAttachOutput,
};
