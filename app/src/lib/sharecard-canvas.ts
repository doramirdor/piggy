// Canvas2D renderer for the share card, drawn in the v2 "Statement" identity:
// cool stock, hairline rules, a Hoefler figure carrying the whole composition.
// All text comes from shareCardText() so the honesty rules live in one place.
//
// The exported PNG is deliberately the LIGHT variant. The card is the one
// artefact that leaves the machine, cool statement stock is the distinctive half
// of the identity, and a pale card reads better in a feed of dark ones.

import type { ShareCardData } from "../types";
import { shareCardText } from "./sharecard";

export const CARD_W = 2400;
export const CARD_H = 1260;
const S = 4; // scale vs the 600x315 mockup

// Native macOS faces, matching the app. Canvas needs real family names.
const SERIF = '"Hoefler Text", "Baskerville", Georgia, serif';
const SANS = '-apple-system, BlinkMacSystemFont, "SF Pro Text", "Helvetica Neue", sans-serif';
const MONO = 'ui-monospace, "SF Mono", SFMono-Regular, Menlo, monospace';

const STOCK = "#e6ebef";
const SHEET = "#f4f7f9";
const INK = "#0d1b26";
const INK_2 = "#4d5c68";
const INK_3 = "#5b6873";
const RULE = "#6b7a87";
const MEASURED = "#1c6b48";

function px(n: number): number {
  return n * S;
}

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
export function renderShareCard(data: ShareCardData): HTMLCanvasElement {
  const t = shareCardText(data);
  const canvas = document.createElement("canvas");
  canvas.width = CARD_W;
  canvas.height = CARD_H;
  const ctx = canvas.getContext("2d");
  if (!ctx) return canvas;

  const padX = px(38);
  const padY = px(32);

  // --- the sheet: flat stock, one inset sheet, no glow and no grain ------
  ctx.fillStyle = STOCK;
  ctx.fillRect(0, 0, CARD_W, CARD_H);
  ctx.fillStyle = SHEET;
  ctx.fillRect(px(14), px(14), CARD_W - px(28), CARD_H - px(28));
  ctx.strokeStyle = RULE;
  ctx.lineWidth = Math.max(1, px(0.4));
  ctx.strokeRect(px(14), px(14), CARD_W - px(28), CARD_H - px(28));

  ctx.textBaseline = "middle";

  // --- nameplate row: mark + Hoefler wordmark, week in the right margin ---
  const topY = padY + px(14);
  const u = px(1.35);
  drawPiggyMark(ctx, padX - 4.6 * u, topY - 12.3 * u, u);
  const markW = 16.6 * u + px(8);
  ctx.textAlign = "left";
  ctx.font = `400 ${px(21)}px ${SERIF}`;
  ctx.fillStyle = INK;
  ctx.fillText("Piggy", padX + markW, topY);
  folio(ctx, t.week, CARD_W - padX, topY, px(10.5), INK_3, "right");

  // rule under the nameplate, the way a masthead is ruled off
  const ruleY = topY + px(20);
  ctx.fillStyle = INK;
  ctx.fillRect(padX, ruleY, CARD_W - padX * 2, Math.max(2, px(0.6)));

  // --- the figure IS the picture -----------------------------------------
  // Piggy can never have imagery, so the number carries the scale contrast a
  // magazine gets from a full-bleed photograph.
  const midY = CARD_H / 2 + px(10);
  folio(ctx, t.kicker, padX, midY - px(64), px(11), INK_3);

  ctx.textAlign = "left";
  // Fit to the column rather than trusting one size: `t.big` swings from "1.7x
  // longer" to "1.2M tokens" depending on what Piggy can honestly claim, and a
  // fixed size clipped the longer strings off the edge of the card.
  const bigMax = CARD_W - padX * 2;
  let bigSize = px(112);
  do {
    ctx.font = `400 ${bigSize}px ${SERIF}`;
    if (ctx.measureText(t.big).width <= bigMax) break;
    bigSize -= px(2);
  } while (bigSize > px(40));
  // Green is spent here only when a randomised holdout stands behind the
  // number. Everything else on this card is ink.
  ctx.fillStyle = data.headlineLabel === "measured" ? MEASURED : INK;
  ctx.fillText(t.big, padX, midY);

  ctx.font = `400 ${px(15)}px ${SANS}`;
  ctx.fillStyle = INK_2;
  ctx.fillText(t.sub, padX, midY + px(74));

  // --- footer: ruled off, proof credit left, url right -------------------
  const botY = CARD_H - padY - px(12);
  ctx.fillStyle = RULE;
  ctx.fillRect(padX, botY - px(24), CARD_W - padX * 2, Math.max(1, px(0.3)));
  folio(ctx, t.proof, padX, botY, px(10.5), INK_3);
  ctx.textAlign = "right";
  ctx.font = `600 ${px(12)}px ${SANS}`;
  ctx.fillStyle = INK_2;
  ctx.fillText(t.url, CARD_W - padX, botY);

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
