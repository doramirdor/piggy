// The token system, enforced.
//
// The colour audit that produced the v2 palette only ever read `index.css`, so
// every colour written inside a component survived it untouched: UsageChart and
// StreamBars both kept `#22c55e` on cache write, and SaverIcon gave Sweep a
// green tile sitting next to a chip that said "estimated". A token system
// enforced only in the stylesheet is not enforced.
//
// So this walks the source tree instead of trusting review. It is a test rather
// than an ESLint rule on purpose: the project has no ESLint, `npm test` is the
// gate that already runs, and adding a linter plus its plugin tree to express
// one rule is a lot of dependency for one rule.

import { describe, it, expect } from "vitest";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";

const SRC = new URL("..", import.meta.url).pathname;

/**
 * Files allowed to name a colour outright, each for a reason that cannot be
 * solved with a custom property.
 */
const ALLOW = new Set([
  // Canvas2D has no access to CSS custom properties: `ctx.fillStyle` needs a
  // real value. The share card is the one surface that renders outside the
  // document, and it carries its own copy of the palette with a comment saying
  // so.
  "lib/sharecard-canvas.ts",
]);

/** `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb(...)`, `rgba(...)`. */
const LITERAL = /#[0-9a-fA-F]{3,8}\b|\brgba?\(\s*\d[^)]*\)/g;

/**
 * A CSS custom property with a literal fallback is legitimate everywhere:
 * `var(--coin, #d4a017)` still routes through the token, and the fallback only
 * applies if the mark is rendered outside the app. Strip those before scanning
 * so the rule does not punish the correct pattern.
 */
const VAR_WITH_FALLBACK = /var\(\s*--[a-z0-9-]+\s*,\s*[^)]*\)/gi;

function sourceFiles(dir: string, out: string[] = []): string[] {
  for (const name of readdirSync(dir)) {
    const full = join(dir, name);
    if (statSync(full).isDirectory()) {
      sourceFiles(full, out);
    } else if (/\.tsx?$/.test(name) && !/\.test\.tsx?$/.test(name)) {
      out.push(full);
    }
  }
  return out;
}

describe("palette", () => {
  it("has no hard-coded colour outside the files that cannot use tokens", () => {
    const offenders: string[] = [];

    for (const file of sourceFiles(SRC)) {
      const rel = relative(SRC, file);
      if (ALLOW.has(rel)) continue;

      const scannable = readFileSync(file, "utf8").replace(VAR_WITH_FALLBACK, "");
      for (const hit of scannable.match(LITERAL) ?? []) {
        offenders.push(`${rel}: ${hit}`);
      }
    }

    expect(
      offenders,
      `Hard-coded colour found. Use a token from index.css (--ink, --measured, ` +
        `--cat-1..4, …). Categorical data takes the --cat ramp; --measured is ` +
        `reserved for a claim a randomised holdout backs. If the colour genuinely ` +
        `cannot be a custom property, add the file to ALLOW with the reason.\n` +
        offenders.join("\n"),
    ).toEqual([]);
  });

  it("still permits a token with a literal fallback", () => {
    // The mark has to render if it is ever extracted from the app, so
    // `var(--coin, #d4a017)` is the correct form and must not trip the rule.
    const mark = readFileSync(join(SRC, "components/PiggyMark.tsx"), "utf8");
    expect(mark).toMatch(/var\(--coin,\s*#/);
    expect(mark.replace(VAR_WITH_FALLBACK, "").match(LITERAL)).toBeNull();
  });
});
