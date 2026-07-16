#!/usr/bin/env node
/**
 * Capture the README screenshots from the mock UI.
 *
 * Shoots against `npm run dev:mock` (VITE_MOCK=1), never a real install: the
 * fixtures are designed for this and no real session data, project name, or
 * token count can leak into a public repo.
 *
 * Drives headless Chrome over the DevTools Protocol using Node's built-in
 * WebSocket (Node >= 22), so there is no Playwright/Puppeteer dependency to
 * install or keep current. The tab is store state rather than a route, so each
 * shot clicks the sidebar item and waits for the paint.
 *
 * Usage:
 *   npm --prefix app run dev:mock     # in one shell
 *   node app/scripts/screenshots.mjs  # in another
 *
 * Output: docs/screenshots/<tab>.png at 2x (retina-crisp on GitHub).
 */
import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const appDir = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = resolve(appDir, "..");
const outDir = join(repoRoot, "docs", "screenshots");

const URL_BASE = process.env.PIGGY_MOCK_URL || "http://localhost:5173";
const PORT = 9222;
const CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

/** Sidebar label -> output filename. Order matches the sidebar. */
const SHOTS = [
  { label: "Dashboard", file: "dashboard" },
  { label: "Savers", file: "savers" },
  { label: "Discovery", file: "discovery" },
  { label: "Proof", file: "proof" },
  { label: "Reports", file: "reports" },
  { label: "Settings", file: "settings" },
];

const VIEWPORT = { width: 1280, height: 880, deviceScaleFactor: 2 };

/** Backdrop kept around the app window, for its rounded corners + drop shadow. */
const PAD = 36;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function getJSON(path, method = "GET") {
  // Chrome requires PUT for /json/new and rejects GET with a plain-text error.
  const res = await fetch(`http://127.0.0.1:${PORT}${path}`, { method });
  return res.json();
}

/** Minimal CDP client over one WebSocket. */
class CDP {
  constructor(ws) {
    this.ws = ws;
    this.id = 0;
    this.pending = new Map();
    ws.addEventListener("message", (ev) => {
      const msg = JSON.parse(ev.data);
      const p = this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      msg.error ? p.reject(new Error(JSON.stringify(msg.error))) : p.resolve(msg.result);
    });
  }
  send(method, params = {}) {
    const id = ++this.id;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }
  static connect(url) {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(url);
      ws.addEventListener("open", () => resolve(new CDP(ws)));
      ws.addEventListener("error", reject);
    });
  }
}

/** Click a sidebar nav item by its visible label. */
const clickNav = (label) => `
  (() => {
    const b = [...document.querySelectorAll('.nav-item')].find(
      (el) => el.querySelector('.ni-label')?.textContent?.trim() === ${JSON.stringify(label)}
    );
    if (!b) return 'missing:' + ${JSON.stringify(label)};
    b.click();
    return 'ok';
  })()
`;

/**
 * The mock renders the app at its real window size (940x660) on a desktop
 * backdrop, so shoot that element rather than the viewport: a viewport shot
 * carries a slab of empty backdrop that reads as dead space on GitHub.
 * Scrolling inside the window is genuine app behavior and stays as-is.
 */
const PANEL_RECT = `
  (() => {
    const el = document.querySelector('.sidebar')?.parentElement;
    if (!el) return null;
    const r = el.getBoundingClientRect();
    return { x: r.x, y: r.y, width: r.width, height: r.height };
  })()
`;

async function main() {
  // Fail early and clearly if the mock server is not up.
  try {
    await fetch(URL_BASE);
  } catch {
    console.error(`No mock server at ${URL_BASE}. Start it first:\n  npm --prefix app run dev:mock`);
    process.exit(1);
  }

  mkdirSync(outDir, { recursive: true });

  const chrome = spawn(CHROME, [
    "--headless=new",
    `--remote-debugging-port=${PORT}`,
    "--hide-scrollbars",
    "--force-color-profile=srgb",
    "--user-data-dir=" + join(repoRoot, "target", "chrome-screenshots"),
    "about:blank",
  ]);
  chrome.on("error", (e) => {
    console.error("failed to launch Chrome:", e.message);
    process.exit(1);
  });

  try {
    // Wait for the debugger endpoint.
    for (let i = 0; ; i++) {
      try {
        await getJSON("/json/version");
        break;
      } catch {
        if (i > 100) throw new Error("Chrome DevTools endpoint never came up");
        await sleep(100);
      }
    }

    const target = await getJSON(`/json/new?${encodeURIComponent(URL_BASE)}`, "PUT");
    const cdp = await CDP.connect(target.webSocketDebuggerUrl);

    await cdp.send("Page.enable");
    await cdp.send("Runtime.enable");
    await cdp.send("Emulation.setDeviceMetricsOverride", { ...VIEWPORT, mobile: false });
    // The mock's dark desktop backdrop is part of the shot; keep it opaque.
    await cdp.send("Emulation.setEmulatedMedia", {
      features: [{ name: "prefers-color-scheme", value: "dark" }],
    });
    await cdp.send("Page.navigate", { url: URL_BASE });
    await sleep(2500); // fonts + first data paint

    const { result: rect } = await cdp.send("Runtime.evaluate", {
      expression: PANEL_RECT,
      returnByValue: true,
    });
    if (!rect.value) throw new Error("could not find the app panel to clip to");
    const clip = {
      x: Math.max(0, rect.value.x - PAD),
      y: Math.max(0, rect.value.y - PAD),
      width: rect.value.width + PAD * 2,
      height: rect.value.height + PAD * 2,
      scale: 1,
    };

    for (const { label, file } of SHOTS) {
      const { result } = await cdp.send("Runtime.evaluate", {
        expression: clickNav(label),
        returnByValue: true,
      });
      if (result.value !== "ok") throw new Error(`could not open tab: ${result.value}`);
      await sleep(900); // tab render + any async fetch in the mock

      const { data } = await cdp.send("Page.captureScreenshot", {
        format: "png",
        captureBeyondViewport: false,
        clip,
      });
      const dest = join(outDir, `${file}.png`);
      writeFileSync(dest, Buffer.from(data, "base64"));
      console.log(`captured ${file}.png`);
    }
  } finally {
    chrome.kill();
  }

  // Keep the committed PNGs small: GitHub serves them on every README view.
  try {
    execFileSync("which", ["pngquant"], { stdio: "ignore" });
    for (const { file } of SHOTS) {
      execFileSync("pngquant", ["--force", "--skip-if-larger", "--quality", "65-90",
        "--output", join(outDir, `${file}.png`), join(outDir, `${file}.png`)]);
    }
    console.log("compressed with pngquant");
  } catch {
    console.log("pngquant not installed; skipping compression (brew install pngquant)");
  }

  console.log(`\ndone: ${outDir}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
