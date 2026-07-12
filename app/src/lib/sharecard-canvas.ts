// Canvas2D renderer for the share card — draws docs/mockups/sharecard.html at
// 2400×1260 (4× the 600×315 mockup) with no html2canvas dependency. All text
// comes from shareCardText() so the honesty rules live in one place.

import type { ShareCardData } from "../types";
import { shareCardText } from "./sharecard";

export const CARD_W = 2400;
export const CARD_H = 1260;
const S = 4; // scale vs the 600×315 mockup

const FONT_STACK =
  '-apple-system, BlinkMacSystemFont, "SF Pro Display", "Helvetica Neue", sans-serif';

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

  // --- background: base + two corner glows -------------------------------
  ctx.fillStyle = "#101013";
  ctx.fillRect(0, 0, CARD_W, CARD_H);

  const glowGreen = ctx.createRadialGradient(
    CARD_W * 0.85,
    -CARD_H * 0.1,
    0,
    CARD_W * 0.85,
    -CARD_H * 0.1,
    CARD_W * 0.85,
  );
  glowGreen.addColorStop(0, "rgba(48,209,88,0.28)");
  glowGreen.addColorStop(0.55, "rgba(48,209,88,0)");
  ctx.fillStyle = glowGreen;
  ctx.fillRect(0, 0, CARD_W, CARD_H);

  const glowBlue = ctx.createRadialGradient(
    -CARD_W * 0.05,
    CARD_H * 1.1,
    0,
    -CARD_W * 0.05,
    CARD_H * 1.1,
    CARD_W * 0.9,
  );
  glowBlue.addColorStop(0, "rgba(10,132,255,0.22)");
  glowBlue.addColorStop(0.55, "rgba(10,132,255,0)");
  ctx.fillStyle = glowBlue;
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
  ctx.font = `400 ${px(22)}px ${FONT_STACK}`;
  ctx.fillText("🐷", padX, topY);
  const pigW = px(30);
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
  grad.addColorStop(0, "#30d158");
  grad.addColorStop(1, "#66e08d");
  ctx.fillStyle = grad;
  ctx.fillText(t.big, padX, bigY);
  setLetterSpacing(ctx, 0);

  ctx.font = `500 ${px(15)}px ${FONT_STACK}`;
  ctx.fillStyle = "rgba(255,255,255,0.62)";
  ctx.fillText(t.sub, padX, midCenter + px(52));

  // --- bottom row: proof + url ------------------------------------------
  const botY = CARD_H - padBottom - px(6);
  ctx.beginPath();
  ctx.fillStyle = "#30d158";
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
