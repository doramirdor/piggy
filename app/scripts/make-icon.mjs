// Renders the macOS-shaped app icon master (icons/icon-source.png, 1024).
// Apple's grid: 1024 canvas, 824 superellipse, the rest is shadow room.
// Regenerate the platform set with: npx tauri icon src-tauri/icons/icon-source.png
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const NAVY = "#01132e";
const GOLD = "#d4a017";

// Superellipse |x/a|^n + |y/a|^n = 1. n=5 is the Big Sur corner.
const squircle = (cx, cy, a, n = 5, steps = 512) => {
  const pts = [];
  for (let i = 0; i < steps; i++) {
    const t = (i / steps) * 2 * Math.PI;
    const c = Math.cos(t), s = Math.sin(t);
    pts.push([
      cx + a * Math.sign(c) * Math.abs(c) ** (2 / n),
      cy + a * Math.sign(s) * Math.abs(s) ** (2 / n),
    ]);
  }
  return `M${pts.map(([x, y]) => `${x.toFixed(2)} ${y.toFixed(2)}`).join("L")}Z`;
};

const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024" width="1024" height="1024">
  <defs>
    <linearGradient id="plate" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#0a2450"/>
      <stop offset="1" stop-color="${NAVY}"/>
    </linearGradient>
    <linearGradient id="sheen" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#ffffff" stop-opacity="0.15"/>
      <stop offset="0.42" stop-color="#ffffff" stop-opacity="0"/>
    </linearGradient>
    <filter id="cast" x="-25%" y="-25%" width="150%" height="150%">
      <feDropShadow dx="0" dy="14" stdDeviation="18" flood-color="#000" flood-opacity="0.34"/>
    </filter>
  </defs>

  <g filter="url(#cast)">
    <path d="${squircle(512, 512, 412)}" fill="url(#plate)"/>
    <path d="${squircle(512, 512, 412)}" fill="url(#sheen)"/>
    <path d="${squircle(512, 512, 412)}" fill="none" stroke="#ffffff" stroke-opacity="0.10" stroke-width="3"/>
  </g>

  <g transform="translate(512 512)">
    <circle r="292" fill="none" stroke="${GOLD}" stroke-width="20" opacity="0.85"/>
    <g fill="${GOLD}" stroke="${GOLD}" stroke-width="26" stroke-linejoin="round">
      <path d="M-190 -60 L-128 -190 L-40 -132 Z"/>
      <path d="M190 -60 L128 -190 L40 -132 Z"/>
      <ellipse cx="0" cy="40" rx="196" ry="174" stroke="none"/>
    </g>
    <ellipse cx="0" cy="88" rx="92" ry="70" fill="${NAVY}"/>
    <ellipse cx="-32" cy="88" rx="17" ry="24" fill="${GOLD}"/>
    <ellipse cx="32" cy="88" rx="17" ry="24" fill="${GOLD}"/>
    <circle cx="-96" cy="-24" r="23" fill="${NAVY}"/>
    <circle cx="96" cy="-24" r="23" fill="${NAVY}"/>
  </g>
</svg>
`;

const svgPath = join(root, "src-tauri/icons/icon-source.svg");
const pngPath = join(root, "src-tauri/icons/icon-source.png");
writeFileSync(svgPath, svg);
execFileSync("rsvg-convert", ["-w", "1024", "-h", "1024", svgPath, "-o", pngPath]);
console.log(pngPath);
