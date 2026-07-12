// Canvas2D renderer for the share card — draws docs/mockups/sharecard.html at
// 2400×1260 (4× the 600×315 mockup) with no html2canvas dependency. All text
// comes from shareCardText() so the honesty rules live in one place.

import type { ShareCardData } from "../types";
import { shareCardText } from "./sharecard";

export const CARD_W = 2400;
export const CARD_H = 1260;
const S = 4; // scale vs the 600×315 mockup

const FONT_STACK =
  '"Inter Variable", "Inter", -apple-system, BlinkMacSystemFont, "SF Pro Display", "Helvetica Neue", sans-serif';

function px(n: number): number {
  return n * S;
}

/** Render the card to a fresh canvas element. */
export function renderShareCard(data: ShareCardData): HTMLCanvasElement {
  const t = shareCardText(data);
  const canvas = document.createElement("canvas");
  canvas.width = CARD_W;
  canvas.height = CARD_H;
  const ctx = canvas.getContext("2d");
  if (!ctx) return canvas;

  // --- background: base + two corner glows (v1.0: green + brand pink) ----
  ctx.fillStyle = "#06080b";
  ctx.fillRect(0, 0, CARD_W, CARD_H);

  const glowGreen = ctx.createRadialGradient(
    CARD_W * 0.85,
    -CARD_H * 0.1,
    0,
    CARD_W * 0.85,
    -CARD_H * 0.1,
    CARD_W * 0.85,
  );
  glowGreen.addColorStop(0, "rgba(34,197,94,0.30)");
  glowGreen.addColorStop(0.55, "rgba(34,197,94,0)");
  ctx.fillStyle = glowGreen;
  ctx.fillRect(0, 0, CARD_W, CARD_H);

  const glowPink = ctx.createRadialGradient(
    -CARD_W * 0.05,
    CARD_H * 1.1,
    0,
    -CARD_W * 0.05,
    CARD_H * 1.1,
    CARD_W * 0.9,
  );
  glowPink.addColorStop(0, "rgba(255,125,168,0.18)");
  glowPink.addColorStop(0.55, "rgba(255,125,168,0)");
  ctx.fillStyle = glowPink;
  ctx.fillRect(0, 0, CARD_W, CARD_H);

  // --- subtle dot grain --------------------------------------------------
  ctx.fillStyle = "rgba(255,255,255,0.045)";
  const step = px(22);
  const r = 1.6;
  for (let y = 0; y < CARD_H; y += step) {
    for (let x = 0; x < CARD_W; x += step) {
      ctx.beginPath();
      ctx.arc(x, y, r, 0, Math.PI * 2);
      ctx.fill();
    }
  }

  const padX = px(38);
  const padTop = px(34);
  const padBottom = px(34);

  // --- top row: pig + name + week ---------------------------------------
  ctx.textBaseline = "middle";
  const topY = padTop + px(11);
  ctx.textAlign = "left";
  // Vector piggy mark (same shape as components/PiggyMark) instead of the 🐷
  // emoji, so the share card renders identically on every platform.
  const u = px(1.55);
  drawPiggyMark(ctx, padX - 4.6 * u, topY - 12.3 * u, u);
  const pigW = 16.6 * u + px(9);
  ctx.font = `700 ${px(16)}px ${FONT_STACK}`;
  ctx.fillStyle = "rgba(255,255,255,0.94)";
  ctx.fillText("Piggy", padX + pigW, topY);
  ctx.textAlign = "right";
  ctx.font = `500 ${px(12)}px ${FONT_STACK}`;
  ctx.fillStyle = "rgba(255,255,255,0.45)";
  ctx.fillText(t.week, CARD_W - padX, topY);

  // --- middle block: kicker / big / sub ---------------------------------
  ctx.textAlign = "left";
  const midCenter = CARD_H / 2;

  ctx.font = `500 ${px(14)}px ${FONT_STACK}`;
  ctx.fillStyle = "rgba(255,255,255,0.55)";
  ctx.fillText(t.kicker, padX, midCenter - px(46));

  // big headline with a green gradient fill
  setLetterSpacing(ctx, px(-2.5));
  ctx.font = `800 ${px(64)}px ${FONT_STACK}`;
  const bigY = midCenter + px(6);
  const grad = ctx.createLinearGradient(padX, 0, padX + px(360), 0);
  grad.addColorStop(0, "#22c55e");
  grad.addColorStop(1, "#4ade80");
  ctx.fillStyle = grad;
  ctx.fillText(t.big, padX, bigY);
  setLetterSpacing(ctx, 0);

  ctx.font = `500 ${px(15)}px ${FONT_STACK}`;
  ctx.fillStyle = "rgba(255,255,255,0.62)";
  ctx.fillText(t.sub, padX, midCenter + px(52));

  // --- bottom row: proof + url ------------------------------------------
  const botY = CARD_H - padBottom - px(6);
  ctx.beginPath();
  ctx.fillStyle = "#22c55e";
  ctx.arc(padX + px(2.5), botY, px(2.6), 0, Math.PI * 2);
  ctx.fill();
  ctx.textAlign = "left";
  ctx.font = `400 ${px(11.5)}px ${FONT_STACK}`;
  ctx.fillStyle = "rgba(255,255,255,0.45)";
  ctx.fillText(t.proof, padX + px(11), botY);
  ctx.textAlign = "right";
  ctx.font = `600 ${px(12)}px ${FONT_STACK}`;
  ctx.fillStyle = "rgba(255,255,255,0.55)";
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

/** roundRect polyfill — Safari/WKWebView versions Piggy targets may lack it. */
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

/** Some engines gate `letterSpacing`; set it only when supported. */
function setLetterSpacing(ctx: CanvasRenderingContext2D, value: number): void {
  const c = ctx as CanvasRenderingContext2D & { letterSpacing?: string };
  if ("letterSpacing" in c) {
    c.letterSpacing = `${value}px`;
  }
}

/** Base64 PNG (no data-URL prefix) — the payload for the save-to-Desktop command. */
export function canvasToPngBase64(canvas: HTMLCanvasElement): string {
  const url = canvas.toDataURL("image/png");
  return url.slice(url.indexOf(",") + 1);
}

/** A PNG blob for clipboard writes. */
export function canvasToPngBlob(canvas: HTMLCanvasElement): Promise<Blob | null> {
  return new Promise((resolve) => canvas.toBlob(resolve, "image/png"));
}
