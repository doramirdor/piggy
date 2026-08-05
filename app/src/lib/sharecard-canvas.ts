// Canvas2D renderer for the share card, drawn in the v2 "Statement" identity:
// cool stock, hairline rules, a Hoefler figure carrying the whole composition.
// All text comes from shareCardText() so the honesty rules live in one place.
//
// Light is the default variant: the card is the one artefact that leaves the
// machine, cool statement stock is the distinctive half of the identity, and a
// pale card reads better in a feed of dark ones. Dark is offered, not assumed.

import type { ShareCardData } from "../types";
import { shareCardText } from "./sharecard";

export interface ShareCardOptions {
  theme: "light" | "dark";
  /** Logical export size; the PNG is drawn at 2x for retina. */
  width: number;
  height: number;
  /** Which parts of the claim the card carries. */
  include: { percent: boolean; totals: boolean; method: boolean };
}

/** Export presets. The first is the default. */
export const CARD_SIZES = [
  { label: "1200 × 630 (Twitter / X)", width: 1200, height: 630 },
  { label: "1080 × 1080 (Square)", width: 1080, height: 1080 },
];

export const DEFAULT_SHARE_OPTIONS: ShareCardOptions = {
  theme: "light",
  width: CARD_SIZES[0].width,
  height: CARD_SIZES[0].height,
  include: { percent: true, totals: true, method: true },
};

// Native macOS faces, matching the app. Canvas needs real family names.
const SERIF = '"Hoefler Text", "Baskerville", Georgia, serif';
const SANS = '-apple-system, BlinkMacSystemFont, "SF Pro Text", "Helvetica Neue", sans-serif';
const MONO = 'ui-monospace, "SF Mono", SFMono-Regular, Menlo, monospace';

// Both palettes are copied from index.css by hand: Canvas2D cannot read custom
// properties, which is why this file is in the palette test's ALLOW list.
const PALETTE = {
  light: {
    stock: "#e6ebef",
    sheet: "#f4f7f9",
    ink: "#0d1b26",
    ink2: "#4d5c68",
    ink3: "#5b6873",
    rule: "#6b7a87",
    measured: "#1c6b48",
  },
  dark: {
    stock: "#070d13",
    sheet: "#101a23",
    ink: "#e9eef2",
    ink2: "#9aa8b4",
    ink3: "#7d8d99",
    rule: "#697a85",
    measured: "#3fae7a",
  },
};

/** Tracked-out uppercase, the margin voice. Canvas has no letter-spacing on
 *  older WKWebView builds, so this draws glyph by glyph and returns the width. */
function folio(
  ctx: CanvasRenderingContext2D,
  text: string,
  x: number,
  y: number,
  size: number,
  color: string,
  align: "left" | "right" = "left",
): number {
  const chars = text.toUpperCase().split("");
  const track = size * 0.18;
  ctx.font = `600 ${size}px ${MONO}`;
  const total = chars.reduce((w, c) => w + ctx.measureText(c).width + track, 0) - track;
  let cx = align === "right" ? x - total : x;
  ctx.fillStyle = color;
  ctx.textAlign = "left";
  for (const c of chars) {
    ctx.fillText(c, cx, y);
    cx += ctx.measureText(c).width + track;
  }
  return total;
}

/** Render the card to a fresh canvas element. */
export function renderShareCard(
  data: ShareCardData,
  opts: ShareCardOptions = DEFAULT_SHARE_OPTIONS,
): HTMLCanvasElement {
  const t = shareCardText(data);
  const canvas = document.createElement("canvas");
  const W = opts.width * 2;
  const H = opts.height * 2;
  canvas.width = W;
  canvas.height = H;
  const ctx = canvas.getContext("2d");
  if (!ctx) return canvas;

  // Type and rules scale off the width, so the 600x315 mockup's proportions
  // survive any preset; the vertical anchors are read off the real height.
  const S = W / 600;
  const px = (n: number) => n * S;
  const c = PALETTE[opts.theme];
  const inc = opts.include;

  const padX = px(38);
  const padY = px(32);

  // --- the sheet: flat stock, one inset sheet, no glow and no grain ------
  ctx.fillStyle = c.stock;
  ctx.fillRect(0, 0, W, H);
  ctx.fillStyle = c.sheet;
  ctx.fillRect(px(14), px(14), W - px(28), H - px(28));
  ctx.strokeStyle = c.rule;
  ctx.lineWidth = Math.max(1, px(0.4));
  ctx.strokeRect(px(14), px(14), W - px(28), H - px(28));

  ctx.textBaseline = "middle";

  // --- nameplate row: mark + Hoefler wordmark, week in the right margin ---
  const topY = padY + px(14);
  const u = px(1.35);
  drawPiggyMark(ctx, padX - 4.6 * u, topY - 12.3 * u, u);
  const markW = 16.6 * u + px(8);
  ctx.textAlign = "left";
  ctx.font = `400 ${px(21)}px ${SERIF}`;
  ctx.fillStyle = c.ink;
  ctx.fillText("Piggy", padX + markW, topY);
  if (inc.method) folio(ctx, t.week, W - padX, topY, px(10.5), c.ink3, "right");

  // rule under the nameplate, the way a masthead is ruled off
  const ruleY = topY + px(20);
  ctx.fillStyle = c.ink;
  ctx.fillRect(padX, ruleY, W - padX * 2, Math.max(2, px(0.6)));

  // --- the figure IS the picture -----------------------------------------
  // Piggy can never have imagery, so the number carries the scale contrast a
  // magazine gets from a full-bleed photograph.
  const midY = H / 2 + px(10);
  if (inc.totals) {
    folio(ctx, t.kicker, padX, midY - px(64), px(11), c.ink3);

    ctx.textAlign = "left";
    // Fit to the column rather than trusting one size: `t.big` swings from "1.7x
    // longer" to "1.2M tokens" depending on what Piggy can honestly claim, and a
    // fixed size clipped the longer strings off the edge of the card.
    const bigMax = W - padX * 2;
    let bigSize = px(112);
    do {
      ctx.font = `400 ${bigSize}px ${SERIF}`;
      if (ctx.measureText(t.big).width <= bigMax) break;
      bigSize -= px(2);
    } while (bigSize > px(40));
    // Green is spent here only when a randomised holdout stands behind the
    // number. Everything else on this card is ink.
    ctx.fillStyle = data.headlineLabel === "measured" ? c.measured : c.ink;
    ctx.fillText(t.big, padX, midY);
  }

  if (inc.percent) {
    ctx.textAlign = "left";
    ctx.font = `400 ${px(15)}px ${SANS}`;
    ctx.fillStyle = c.ink2;
    // Without the figure above it the multiplier is the claim, so it moves up
    // into the optical centre instead of hanging under empty space.
    ctx.fillText(t.sub, padX, inc.totals ? midY + px(74) : midY);
  }

  // --- footer: ruled off, proof credit left, url right -------------------
  const botY = H - padY - px(12);
  ctx.fillStyle = c.rule;
  ctx.fillRect(padX, botY - px(24), W - padX * 2, Math.max(1, px(0.3)));
  if (inc.method) folio(ctx, t.proof, padX, botY, px(10.5), c.ink3);
  ctx.textAlign = "right";
  ctx.font = `600 ${px(12)}px ${SANS}`;
  ctx.fillStyle = c.ink2;
  ctx.fillText(t.url, W - padX, botY);

  return canvas;
}

/** Draw the Piggy brand mark at (ox, oy) with `u` pixels per SVG unit. Mirrors
 *  the geometry of components/PiggyMark (24×24 viewBox). */
function drawPiggyMark(
  ctx: CanvasRenderingContext2D,
  ox: number,
  oy: number,
  u: number,
): void {
  const X = (v: number) => ox + v * u;
  const Y = (v: number) => oy + v * u;

  // coin (rotated -14° about its center) + star
  ctx.save();
  ctx.translate(X(12), Y(6.2));
  ctx.rotate((-14 * Math.PI) / 180);
  ctx.beginPath();
  ctx.arc(0, 0, 2.5 * u, 0, Math.PI * 2);
  ctx.fillStyle = "#ffd60a";
  ctx.fill();
  const star: [number, number][] = [
    [0, -1.5], [0.55, -0.35], [1.8, -0.25], [0.85, 0.55], [1.15, 1.8],
    [0, 1.1], [-1.15, 1.8], [-0.85, 0.55], [-1.8, -0.25], [-0.55, -0.35],
  ];
  ctx.beginPath();
  star.forEach(([sx, sy], i) => {
    const px2 = sx * u;
    const py2 = sy * u;
    if (i === 0) ctx.moveTo(px2, py2);
    else ctx.lineTo(px2, py2);
  });
  ctx.closePath();
  ctx.fillStyle = "#c8930a";
  ctx.fill();
  ctx.restore();

  // tail
  ctx.beginPath();
  ctx.moveTo(X(6.1), Y(14.3));
  ctx.quadraticCurveTo(X(4.7), Y(14.1), X(4.95), Y(12.8));
  ctx.quadraticCurveTo(X(5.15), Y(11.7), X(6.35), Y(12.0));
  ctx.strokeStyle = "#ee5a7d";
  ctx.lineWidth = 0.9 * u;
  ctx.lineCap = "round";
  ctx.stroke();

  // body
  ctx.beginPath();
  ctx.ellipse(X(12.1), Y(14.4), 6.9 * u, 5.3 * u, 0, 0, Math.PI * 2);
  ctx.fillStyle = "#ff7da8";
  ctx.fill();

  // ear
  ctx.beginPath();
  ctx.moveTo(X(14.1), Y(9.2));
  ctx.quadraticCurveTo(X(14.9), Y(7.8), X(16.4), Y(7.9));
  ctx.quadraticCurveTo(X(16.6), Y(9.4), X(15.5), Y(10.3));
  ctx.closePath();
  ctx.fillStyle = "#ee5a7d";
  ctx.fill();

  // coin slot
  ctx.beginPath();
  ctx.ellipse(X(12), Y(10.15), 1.9 * u, 0.45 * u, 0, 0, Math.PI * 2);
  ctx.fillStyle = "#b45a72";
  ctx.fill();

  // snout + nostrils
  ctx.beginPath();
  ctx.ellipse(X(17.9), Y(14.6), 2 * u, 1.55 * u, 0, 0, Math.PI * 2);
  ctx.fillStyle = "#ee5a7d";
  ctx.fill();
  ctx.fillStyle = "#8f3a52";
  ctx.beginPath();
  ctx.ellipse(X(17.35), Y(14.6), 0.32 * u, 0.55 * u, 0, 0, Math.PI * 2);
  ctx.fill();
  ctx.beginPath();
  ctx.ellipse(X(18.5), Y(14.6), 0.32 * u, 0.55 * u, 0, 0, Math.PI * 2);
  ctx.fill();

  // eye
  ctx.beginPath();
  ctx.arc(X(15.2), Y(12.4), 0.72 * u, 0, Math.PI * 2);
  ctx.fillStyle = "#301820";
  ctx.fill();

  // legs
  ctx.fillStyle = "#ee5a7d";
  for (const lx of [9, 13.4]) {
    roundRectPath(ctx, X(lx), Y(18.7), 1.7 * u, 1.9 * u, 0.8 * u);
    ctx.fill();
  }
}

/** roundRect polyfill - Safari/WKWebView versions Piggy targets may lack it. */
function roundRectPath(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  r: number,
): void {
  ctx.beginPath();
  ctx.moveTo(x + r, y);
  ctx.arcTo(x + w, y, x + w, y + h, r);
  ctx.arcTo(x + w, y + h, x, y + h, r);
  ctx.arcTo(x, y + h, x, y, r);
  ctx.arcTo(x, y, x + w, y, r);
  ctx.closePath();
}

/** Base64 PNG (no data-URL prefix) - the payload for the save-to-Desktop command. */
export function canvasToPngBase64(canvas: HTMLCanvasElement): string {
  const url = canvas.toDataURL("image/png");
  return url.slice(url.indexOf(",") + 1);
}

/** A PNG blob for clipboard writes. */
export function canvasToPngBlob(canvas: HTMLCanvasElement): Promise<Blob | null> {
  return new Promise((resolve) => canvas.toBlob(resolve, "image/png"));
}
