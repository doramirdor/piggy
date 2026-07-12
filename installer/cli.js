#!/usr/bin/env node
'use strict';

const https = require('https');
const fs = require('fs');
const os = require('os');
const path = require('path');
const crypto = require('crypto');
const readline = require('readline');
const { execFile } = require('child_process');

const {
  mapNodeArchToAssetArch,
  resolveConfig,
  buildReleaseApiUrl,
  findDmgAsset,
  findChecksumsAsset,
  parseChecksumsFile,
  getExpectedChecksum,
  hashesMatch,
  buildNoReleaseMessage,
  isNotFoundStatus,
  formatBytes,
  parseHdiutilAttachOutput,
} = require('./lib');

const pkg = require('./package.json');

// ---------------------------------------------------------------------------
// Friendly, dependency-free output helpers (ANSI escapes, no chalk).
// ---------------------------------------------------------------------------

const isTTY = Boolean(process.stdout.isTTY);
const c = isTTY
  ? {
      reset: '\x1b[0m',
      bold: '\x1b[1m',
      dim: '\x1b[2m',
      red: '\x1b[31m',
      green: '\x1b[32m',
      yellow: '\x1b[33m',
      cyan: '\x1b[36m',
      pink: '\x1b[95m',
    }
  : { reset: '', bold: '', dim: '', red: '', green: '', yellow: '', cyan: '', pink: '' };

function info(msg) {
  console.log(`${c.cyan}${msg}${c.reset}`);
}
function success(msg) {
  console.log(`${c.green}${msg}${c.reset}`);
}
function fail(msg) {
  console.error(`${c.red}${msg}${c.reset}`);
}
function piggy(msg) {
  console.log(`${c.pink}🐷 ${msg}${c.reset}`);
}

// ---------------------------------------------------------------------------
// Small IO helpers (network / process / prompts). Kept out of lib.js on
// purpose so lib.js stays pure and testable without a network connection.
// ---------------------------------------------------------------------------

function httpGet(url, redirectsLeft = 5) {
  return new Promise((resolve, reject) => {
    const req = https.get(
      url,
      { headers: { 'User-Agent': 'piggybank-installer', Accept: 'application/vnd.github+json' } },
      (res) => {
        if (
          res.statusCode >= 300 &&
          res.statusCode < 400 &&
          res.headers.location &&
          redirectsLeft > 0
        ) {
          res.resume();
          return resolve(httpGet(res.headers.location, redirectsLeft - 1));
        }
        let data = '';
        res.setEncoding('utf8');
        res.on('data', (chunk) => (data += chunk));
        res.on('end', () => resolve({ statusCode: res.statusCode, body: data }));
      }
    );
    req.on('error', reject);
  });
}

function downloadWithProgress(url, destPath, redirectsLeft = 5) {
  return new Promise((resolve, reject) => {
    const req = https.get(url, { headers: { 'User-Agent': 'piggybank-installer' } }, (res) => {
      if (
        res.statusCode >= 300 &&
        res.statusCode < 400 &&
        res.headers.location &&
        redirectsLeft > 0
      ) {
        res.resume();
        return resolve(downloadWithProgress(res.headers.location, destPath, redirectsLeft - 1));
      }
      if (res.statusCode !== 200) {
        res.resume();
        return reject(new Error(`Download failed with HTTP status ${res.statusCode}`));
      }

      const total = parseInt(res.headers['content-length'] || '0', 10);
      let downloaded = 0;
      const file = fs.createWriteStream(destPath);

      res.on('data', (chunk) => {
        downloaded += chunk.length;
        if (isTTY) {
          const pct = total ? `${Math.min(100, Math.floor((downloaded / total) * 100))}%` : '';
          process.stdout.write(
            `\r${c.cyan}  downloading… ${pct} (${formatBytes(downloaded)}${
              total ? ` / ${formatBytes(total)}` : ''
            })${c.reset}   `
          );
        }
      });

      res.pipe(file);
      file.on('finish', () => {
        file.close(() => {
          if (isTTY) process.stdout.write('\n');
          resolve();
        });
      });
      file.on('error', reject);
    });
    req.on('error', reject);
  });
}

function sha256HexOfFile(filePath) {
  return new Promise((resolve, reject) => {
    const hash = crypto.createHash('sha256');
    const stream = fs.createReadStream(filePath);
    stream.on('data', (chunk) => hash.update(chunk));
    stream.on('end', () => resolve(hash.digest('hex')));
    stream.on('error', reject);
  });
}

function execFileP(cmd, args, options = {}) {
  return new Promise((resolve, reject) => {
    execFile(cmd, args, { maxBuffer: 20 * 1024 * 1024, ...options }, (err, stdout, stderr) => {
      if (err) {
        err.stdout = stdout;
        err.stderr = stderr;
        return reject(err);
      }
      resolve({ stdout, stderr });
    });
  });
}

function ask(question) {
  return new Promise((resolve) => {
    const rl = readline.createInterface({ input: process.stdin, output: process.stdout });
    rl.question(question, (answer) => {
      rl.close();
      resolve(answer);
    });
  });
}

async function confirm(question) {
  const answer = await ask(question);
  return /^y(es)?$/i.test(answer.trim());
}

async function attachDmg(dmgPath) {
  const { stdout } = await execFileP('hdiutil', ['attach', dmgPath, '-nobrowse']);
  const mountPoint = parseHdiutilAttachOutput(stdout);
  if (!mountPoint) {
    throw new Error(`Could not determine a mount point from hdiutil output:\n${stdout}`);
  }
  return mountPoint;
}

async function detachDmg(mountPoint) {
  try {
    await execFileP('hdiutil', ['detach', mountPoint]);
  } catch (err) {
    // Volume may still be "in use" briefly after Finder/copy activity — retry with force.
    await execFileP('hdiutil', ['detach', mountPoint, '-force']);
  }
}

function findAppInVolume(mountPoint) {
  const entries = fs.readdirSync(mountPoint);
  const app = entries.find((e) => e.toLowerCase().endsWith('.app'));
  return app || null;
}

// ---------------------------------------------------------------------------
// CLI plumbing
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  return {
    yes: argv.includes('--yes') || argv.includes('-y'),
    uninstall: argv.includes('--uninstall'),
    help: argv.includes('--help') || argv.includes('-h'),
  };
}

function printHelp() {
  console.log(`
${c.pink}🐷 piggybank${c.reset} — installer for Piggy, the macOS menu bar app

${c.bold}Usage${c.reset}
  npx piggybank              Install the latest Piggy release
  npx piggybank --yes        Install, skipping the copy-to-/Applications prompt
  npx piggybank --uninstall  Remove /Applications/Piggy.app
  npx piggybank --help       Show this help

${c.bold}Flags${c.reset}
  -y, --yes        Assume "yes" for confirmation prompts
      --uninstall  Uninstall Piggy instead of installing it
  -h, --help       Show this help

${c.bold}Config${c.reset} (read from this package's own package.json "piggy" field)
  piggy.repo     GitHub "<owner>/<repo>" to install from
  piggy.version  Release tag to install, or "latest"
`);
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

async function runInstall(flags) {
  piggy(`${c.bold}Piggy installer${c.reset}`);

  const { repo, version } = resolveConfig(pkg);
  info(`Repo: ${repo}   Version: ${version}`);

  const assetArch = mapNodeArchToAssetArch(os.arch());
  info(`Detected architecture: ${assetArch}`);

  const apiUrl = buildReleaseApiUrl(repo, version);
  info('Checking GitHub for a release…');
  const res = await httpGet(apiUrl);

  if (isNotFoundStatus(res.statusCode)) {
    console.log('');
    piggy(buildNoReleaseMessage(repo));
    console.log('');
    return;
  }

  if (res.statusCode !== 200) {
    throw new Error(
      `GitHub API returned HTTP ${res.statusCode} for ${apiUrl}. ` +
        `(If this repo is private or you're rate-limited, that would explain it.)`
    );
  }

  let release;
  try {
    release = JSON.parse(res.body);
  } catch (e) {
    throw new Error(`Could not parse GitHub's response as JSON: ${e.message}`);
  }

  const assets = release.assets || [];
  const dmgAsset = findDmgAsset(assets, assetArch);
  if (!dmgAsset) {
    throw new Error(
      `No .dmg asset found for architecture "${assetArch}" in release "${
        release.tag_name || version
      }".\n` + `Available assets: ${assets.map((a) => a.name).join(', ') || '(none)'}`
    );
  }
  success(`Found ${dmgAsset.name} (${formatBytes(dmgAsset.size)})`);

  const checksumsAsset = findChecksumsAsset(assets);
  if (!checksumsAsset) {
    throw new Error(
      'This release has no checksums.txt asset, so the download cannot be verified. ' +
        'Refusing to continue — please report this to the Piggy maintainers.'
    );
  }

  info('Fetching checksums…');
  const checksumsRes = await httpGet(checksumsAsset.browser_download_url);
  if (checksumsRes.statusCode !== 200) {
    throw new Error(`Could not download checksums.txt (HTTP ${checksumsRes.statusCode}).`);
  }
  const checksumMap = parseChecksumsFile(checksumsRes.body);
  const expectedHash = getExpectedChecksum(checksumMap, dmgAsset.name);
  if (!expectedHash) {
    throw new Error(
      `checksums.txt does not list a hash for ${dmgAsset.name}. ` +
        'Refusing to continue without verification.'
    );
  }

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'piggybank-'));
  const dmgPath = path.join(tmpDir, dmgAsset.name);

  info(`Downloading ${dmgAsset.name}…`);
  await downloadWithProgress(dmgAsset.browser_download_url, dmgPath);

  info('Verifying checksum…');
  const actualHash = await sha256HexOfFile(dmgPath);
  if (!hashesMatch(actualHash, expectedHash)) {
    fs.rmSync(dmgPath, { force: true });
    throw new Error(
      `Checksum mismatch for ${dmgAsset.name}.\n` +
        `  expected: ${expectedHash}\n` +
        `  actual:   ${actualHash}\n` +
        'The download may be corrupted or tampered with. Aborting — nothing was installed.'
    );
  }
  success('Checksum verified.');

  info('Mounting disk image…');
  const mountPoint = await attachDmg(dmgPath);
  success(`Mounted at ${mountPoint}`);

  try {
    const appName = findAppInVolume(mountPoint);
    if (!appName) {
      throw new Error(`Could not find a .app bundle inside ${mountPoint}`);
    }
    const srcApp = path.join(mountPoint, appName);
    const destApp = path.join('/Applications', appName);

    let shouldCopy = flags.yes;
    if (!shouldCopy) {
      shouldCopy = await confirm(`${c.yellow}Copy ${appName} to /Applications? [y/N] ${c.reset}`);
    }

    if (shouldCopy) {
      info(`Copying ${appName} to /Applications…`);
      if (fs.existsSync(destApp)) {
        fs.rmSync(destApp, { recursive: true, force: true });
      }
      fs.cpSync(srcApp, destApp, { recursive: true, dereference: true });
      success(`Installed to ${destApp}`);

      info('Detaching disk image…');
      await detachDmg(mountPoint);

      console.log('');
      piggy(`${c.bold}All set!${c.reset} Open Piggy from /Applications or Spotlight.`);
    } else {
      info(`Opening ${mountPoint} in Finder — drag ${appName} onto Applications to finish.`);
      await execFileP('open', [mountPoint]);
      await ask(`${c.dim}Press Enter when you're done (this ejects the disk image)… ${c.reset}`);

      info('Detaching disk image…');
      await detachDmg(mountPoint);

      console.log('');
      piggy('Done — thanks for trying Piggy!');
    }
  } catch (err) {
    try {
      await detachDmg(mountPoint);
    } catch (_detachErr) {
      // Original error is more useful than a cleanup failure — swallow this one.
    }
    throw err;
  } finally {
    fs.rmSync(tmpDir, { recursive: true, force: true });
  }
}

// ---------------------------------------------------------------------------
// Uninstall
// ---------------------------------------------------------------------------

async function runUninstall(flags) {
  piggy(`${c.bold}Piggy uninstaller${c.reset}`);

  const appPath = '/Applications/Piggy.app';
  if (!fs.existsSync(appPath)) {
    info(`${appPath} isn't installed — nothing to do.`);
    return;
  }

  let shouldRemove = flags.yes;
  if (!shouldRemove) {
    shouldRemove = await confirm(`${c.yellow}Remove ${appPath}? [y/N] ${c.reset}`);
  }
  if (!shouldRemove) {
    info('Cancelled — Piggy was not removed.');
    return;
  }

  fs.rmSync(appPath, { recursive: true, force: true });
  success(`Removed ${appPath}`);

  console.log('');
  info(`Piggy's saved data at ${c.bold}~/.piggy${c.reset} was left in place on purpose.`);
  info(`To remove that too, run: ${c.bold}rm -rf ~/.piggy${c.reset}`);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

async function main() {
  const flags = parseArgs(process.argv.slice(2));

  if (flags.help) {
    printHelp();
    return;
  }

  if (process.platform !== 'darwin') {
    piggy('Piggy is a macOS menu bar app, so this installer only runs on macOS.');
    info("On another OS there's nothing to install here — grab a Mac, or watch the repo for updates.");
    process.exitCode = 1;
    return;
  }

  if (flags.uninstall) {
    await runUninstall(flags);
    return;
  }

  await runInstall(flags);
}

main().catch((err) => {
  console.log('');
  fail(`Something went wrong: ${err.message}`);
  if (process.env.DEBUG) {
    console.error(err.stack);
  }
  process.exitCode = 1;
});
