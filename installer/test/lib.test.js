'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

const lib = require('../lib');

function fixture(name) {
  return fs.readFileSync(path.join(__dirname, 'fixtures', name), 'utf8');
}

function fixtureJson(name) {
  return JSON.parse(fixture(name));
}

// ---------------------------------------------------------------------------
// mapNodeArchToAssetArch
// ---------------------------------------------------------------------------

test('mapNodeArchToAssetArch: maps arm64 and x64, rejects anything else', () => {
  assert.equal(lib.mapNodeArchToAssetArch('arm64'), 'arm64');
  assert.equal(lib.mapNodeArchToAssetArch('x64'), 'x64');
  assert.throws(() => lib.mapNodeArchToAssetArch('ia32'), /Unsupported architecture/);
  assert.throws(() => lib.mapNodeArchToAssetArch('mips'), /Unsupported architecture/);
});

// ---------------------------------------------------------------------------
// resolveConfig
// ---------------------------------------------------------------------------

test('resolveConfig: falls back to placeholder repo and "latest" when piggy field is absent', () => {
  assert.deepEqual(lib.resolveConfig({}), { repo: 'piggy-app/piggy', version: 'latest' });
  assert.deepEqual(lib.resolveConfig(undefined), { repo: 'piggy-app/piggy', version: 'latest' });
});

test('resolveConfig: reads repo/version from package.json piggy field', () => {
  const pkgJson = { piggy: { repo: 'someorg/somerepo', version: 'v1.2.3' } };
  assert.deepEqual(lib.resolveConfig(pkgJson), { repo: 'someorg/somerepo', version: 'v1.2.3' });
});

test('resolveConfig: partial piggy field fills in the missing half with defaults', () => {
  assert.deepEqual(lib.resolveConfig({ piggy: { repo: 'foo/bar' } }), {
    repo: 'foo/bar',
    version: 'latest',
  });
  assert.deepEqual(lib.resolveConfig({ piggy: { version: 'v9.9.9' } }), {
    repo: 'piggy-app/piggy',
    version: 'v9.9.9',
  });
});

// ---------------------------------------------------------------------------
// buildReleaseApiUrl
// ---------------------------------------------------------------------------

test('buildReleaseApiUrl: "latest" hits the /releases/latest endpoint', () => {
  assert.equal(
    lib.buildReleaseApiUrl('piggy-app/piggy', 'latest'),
    'https://api.github.com/repos/piggy-app/piggy/releases/latest'
  );
  assert.equal(
    lib.buildReleaseApiUrl('piggy-app/piggy', undefined),
    'https://api.github.com/repos/piggy-app/piggy/releases/latest'
  );
});

test('buildReleaseApiUrl: a pinned version hits the /releases/tags/<version> endpoint', () => {
  assert.equal(
    lib.buildReleaseApiUrl('piggy-app/piggy', 'v0.1.0'),
    'https://api.github.com/repos/piggy-app/piggy/releases/tags/v0.1.0'
  );
});

test('buildReleaseApiUrl: rejects a malformed repo', () => {
  assert.throws(() => lib.buildReleaseApiUrl('not-a-repo', 'latest'), /Invalid repo/);
  assert.throws(() => lib.buildReleaseApiUrl('', 'latest'), /Invalid repo/);
  assert.throws(() => lib.buildReleaseApiUrl(undefined, 'latest'), /Invalid repo/);
});

// ---------------------------------------------------------------------------
// findDmgAsset
// ---------------------------------------------------------------------------

test('findDmgAsset: picks the arm64 dmg from a multi-arch release', () => {
  const release = fixtureJson('release-multi-arch.json');
  const asset = lib.findDmgAsset(release.assets, 'arm64');
  assert.equal(asset.name, 'Piggy-0.1.0-arm64.dmg');
});

test('findDmgAsset: picks the x64 dmg from a multi-arch release', () => {
  const release = fixtureJson('release-multi-arch.json');
  const asset = lib.findDmgAsset(release.assets, 'x64');
  assert.equal(asset.name, 'Piggy-0.1.0-x64.dmg');
});

test('findDmgAsset: falls back to the sole dmg when the release only ships one, untagged', () => {
  const release = fixtureJson('release-single-dmg.json');
  const arm = lib.findDmgAsset(release.assets, 'arm64');
  const x64 = lib.findDmgAsset(release.assets, 'x64');
  assert.equal(arm.name, 'Piggy.dmg');
  assert.equal(x64.name, 'Piggy.dmg');
});

test('findDmgAsset: returns null when there is no dmg asset at all', () => {
  const release = fixtureJson('release-no-dmg.json');
  assert.equal(lib.findDmgAsset(release.assets, 'arm64'), null);
});

test('findDmgAsset: handles missing/non-array assets gracefully', () => {
  assert.equal(lib.findDmgAsset(undefined, 'arm64'), null);
  assert.equal(lib.findDmgAsset(null, 'arm64'), null);
});

// ---------------------------------------------------------------------------
// findChecksumsAsset
// ---------------------------------------------------------------------------

test('findChecksumsAsset: finds checksums.txt in a release', () => {
  const release = fixtureJson('release-multi-arch.json');
  const asset = lib.findChecksumsAsset(release.assets);
  assert.equal(asset.name, 'checksums.txt');
});

test('findChecksumsAsset: returns null when the release has no checksums file', () => {
  const release = fixtureJson('release-no-checksums.json');
  assert.equal(lib.findChecksumsAsset(release.assets), null);
});

test('findChecksumsAsset: matches case-insensitively and the singular "checksum.txt" spelling', () => {
  assert.ok(lib.findChecksumsAsset([{ name: 'CHECKSUMS.TXT' }]));
  assert.ok(lib.findChecksumsAsset([{ name: 'checksum.txt' }]));
});

// ---------------------------------------------------------------------------
// parseChecksumsFile / getExpectedChecksum
// ---------------------------------------------------------------------------

test('parseChecksumsFile: parses "hash  filename" lines, skipping comments/blanks', () => {
  const content = fixture('checksums.txt');
  const map = lib.parseChecksumsFile(content);
  assert.equal(map.size, 2);
  assert.equal(
    map.get('Piggy-0.1.0-arm64.dmg'),
    '2ff2fe0948d4a7a2dc4ff8969886df58d37cd5df2375aec52a76bf13f3682288'
  );
  assert.equal(
    map.get('Piggy-0.1.0-x64.dmg'),
    '5682d4959af4fe707b6f019e62ec56b19d45e963ad8999f8b54ed81f34c78bca'
  );
});

test('parseChecksumsFile: also accepts the "*binary" sha256sum -b style prefix', () => {
  const content =
    '2ff2fe0948d4a7a2dc4ff8969886df58d37cd5df2375aec52a76bf13f3682288 *Piggy-0.1.0-arm64.dmg\n';
  const map = lib.parseChecksumsFile(content);
  assert.equal(
    map.get('Piggy-0.1.0-arm64.dmg'),
    '2ff2fe0948d4a7a2dc4ff8969886df58d37cd5df2375aec52a76bf13f3682288'
  );
});

test('parseChecksumsFile: empty/undefined content yields an empty map', () => {
  assert.equal(lib.parseChecksumsFile('').size, 0);
  assert.equal(lib.parseChecksumsFile(undefined).size, 0);
});

test('getExpectedChecksum: exact filename match', () => {
  const map = lib.parseChecksumsFile(fixture('checksums.txt'));
  assert.equal(
    lib.getExpectedChecksum(map, 'Piggy-0.1.0-x64.dmg'),
    '5682d4959af4fe707b6f019e62ec56b19d45e963ad8999f8b54ed81f34c78bca'
  );
});

test('getExpectedChecksum: tolerates a path-prefixed entry', () => {
  const map = lib.parseChecksumsFile(
    '2ff2fe0948d4a7a2dc4ff8969886df58d37cd5df2375aec52a76bf13f3682288  dist/Piggy-0.1.0-arm64.dmg\n'
  );
  assert.equal(
    lib.getExpectedChecksum(map, 'Piggy-0.1.0-arm64.dmg'),
    '2ff2fe0948d4a7a2dc4ff8969886df58d37cd5df2375aec52a76bf13f3682288'
  );
});

test('getExpectedChecksum: returns null when the filename is not listed', () => {
  const map = lib.parseChecksumsFile(fixture('checksums.txt'));
  assert.equal(lib.getExpectedChecksum(map, 'nonexistent.dmg'), null);
});

// ---------------------------------------------------------------------------
// sha256Hex / hashesMatch / verifyChecksum
// ---------------------------------------------------------------------------

test('sha256Hex: matches node:crypto directly for a known buffer', () => {
  const buf = Buffer.from('piggybank installer test payload');
  const expected = crypto.createHash('sha256').update(buf).digest('hex');
  assert.equal(lib.sha256Hex(buf), expected);
});

test('hashesMatch: case-insensitive comparison, false on falsy input', () => {
  assert.equal(lib.hashesMatch('ABCDEF', 'abcdef'), true);
  assert.equal(lib.hashesMatch('abcdef', 'abcdee'), false);
  assert.equal(lib.hashesMatch(null, 'abcdef'), false);
  assert.equal(lib.hashesMatch('abcdef', undefined), false);
});

test('verifyChecksum: true for the correct hash, false for a tampered buffer', () => {
  const buf = Buffer.from('the real dmg bytes');
  const goodHash = crypto.createHash('sha256').update(buf).digest('hex');
  assert.equal(lib.verifyChecksum(buf, goodHash), true);

  const tampered = Buffer.from('the real dmg bytes, but corrupted');
  assert.equal(lib.verifyChecksum(tampered, goodHash), false);
});

// ---------------------------------------------------------------------------
// buildNoReleaseMessage / isNotFoundStatus
// ---------------------------------------------------------------------------

test('buildNoReleaseMessage: friendly message points at the releases page', () => {
  const msg = lib.buildNoReleaseMessage('piggy-app/piggy');
  assert.equal(
    msg,
    "Piggy hasn't shipped its first release yet — watch https://github.com/piggy-app/piggy/releases"
  );
});

test('isNotFoundStatus: only 404 counts', () => {
  assert.equal(lib.isNotFoundStatus(404), true);
  assert.equal(lib.isNotFoundStatus(200), false);
  assert.equal(lib.isNotFoundStatus(500), false);
});

// ---------------------------------------------------------------------------
// formatBytes
// ---------------------------------------------------------------------------

test('formatBytes: formats across units', () => {
  assert.equal(lib.formatBytes(0), '0 B');
  assert.equal(lib.formatBytes(512), '512 B');
  assert.equal(lib.formatBytes(52428800), '50.0 MB');
  assert.equal(lib.formatBytes(1073741824), '1.0 GB');
});

// ---------------------------------------------------------------------------
// parseHdiutilAttachOutput
// ---------------------------------------------------------------------------

test('parseHdiutilAttachOutput: extracts the /Volumes mount point from real-shaped output', () => {
  const output = fixture('hdiutil-attach-output.txt');
  assert.equal(lib.parseHdiutilAttachOutput(output), '/Volumes/Piggy');
});

test('parseHdiutilAttachOutput: returns null when there is no /Volumes line', () => {
  assert.equal(lib.parseHdiutilAttachOutput('/dev/disk4   GUID_partition_scheme\n'), null);
  assert.equal(lib.parseHdiutilAttachOutput(''), null);
  assert.equal(lib.parseHdiutilAttachOutput(undefined), null);
});

test('parseHdiutilAttachOutput: handles volume names with spaces', () => {
  const output = '/dev/disk5s1        \tApple_HFS                      \t/Volumes/Piggy Installer\n';
  assert.equal(lib.parseHdiutilAttachOutput(output), '/Volumes/Piggy Installer');
});
