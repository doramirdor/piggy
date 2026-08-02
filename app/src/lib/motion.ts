// The v2 motion layer. Five moves, all of them things a physical document
// does: a figure settling like a mechanical counter, a rule being drawn, a
// stamp pressed on, a page turning, and a printed progress bar filling.
//
// Nothing bounces and nothing slides in from off-screen. Everything here
// no-ops under `prefers-reduced-motion`.

import { useEffect, useRef, useState } from "react";

export function prefersReducedMotion(): boolean {
  return (
    typeof window !== "undefined" &&
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches
  );
}

/**
 * THE TICK. Counts a figure up to `target` on mount and whenever the target
 * meaningfully changes.
 *
 * Deliberately reserved for the ONE hero figure on a screen. The store
 * refreshes on every session-log write (roughly every couple of seconds while
 * Claude is running), so a ticking number anywhere else would be a permanent
 * flicker rather than a moment.
 */
export function useCountUp(target: number | null, durationMs = 620): number {
  const [value, setValue] = useState(() => (prefersReducedMotion() ? (target ?? 0) : 0));
  const from = useRef(0);
  const raf = useRef<number | null>(null);

  useEffect(() => {
    if (target == null) return;
    if (prefersReducedMotion()) {
      setValue(target);
      return;
    }
    // Animate from wherever the number currently is, so a refresh that nudges
    // the figure eases across the difference instead of restarting from zero.
    const start = from.current;
    const delta = target - start;
    if (Math.abs(delta) < 0.005) {
      setValue(target);
      return;
    }
    let t0: number | null = null;
    const step = (t: number) => {
      if (t0 === null) t0 = t;
      const p = Math.min(1, (t - t0) / durationMs);
      const eased = 1 - Math.pow(1 - p, 3);
      const next = start + delta * eased;
      from.current = next;
      setValue(next);
      if (p < 1) raf.current = requestAnimationFrame(step);
    };
    raf.current = requestAnimationFrame(step);
    return () => {
      if (raf.current !== null) cancelAnimationFrame(raf.current);
    };
  }, [target, durationMs]);

  return target == null ? 0 : value;
}

/**
 * THE STAMP, fired once per earned claim.
 *
 * Three things defeat a naive "animate on mount": App picks the screen with a
 * ternary so every tab switch remounts Proof, the store re-renders it every few
 * seconds off the file watcher, and StrictMode double-invokes effects. So the
 * flag lives outside React, keyed by the identity of the claim itself.
 *
 * ponytail: localStorage, not PiggyState. A "have I already animated this"
 * flag is presentation, not measurement, and losing it when someone clears
 * their app data costs one replayed animation. Move it into `state.rs` only if
 * the moment ever needs to survive that.
 */
const STAMP_KEY = "piggy.stamped.v1";

function readStamped(): Set<string> {
  try {
    const raw = localStorage.getItem(STAMP_KEY);
    return new Set<string>(raw ? (JSON.parse(raw) as string[]) : []);
  } catch {
    return new Set();
  }
}

export function useStampOnce(claimId: string | null): boolean {
  const [fire, setFire] = useState(false);
  useEffect(() => {
    if (!claimId || prefersReducedMotion()) return;
    const seen = readStamped();
    if (seen.has(claimId)) return;
    seen.add(claimId);
    try {
      localStorage.setItem(STAMP_KEY, JSON.stringify([...seen].slice(-50)));
    } catch {
      // A full or disabled store just means the stamp may replay. Harmless.
    }
    setFire(true);
    // Clear the class after the animation so a later re-render does not replay it.
    const id = setTimeout(() => setFire(false), 700);
    return () => clearTimeout(id);
  }, [claimId]);
  return fire;
}

/**
 * THE PAGE. Returns a key that changes whenever `token` changes, for a wrapper
 * whose CSS animation re-runs on a new key. Sections inside it stagger via
 * `--i` (see `.page-turn > *` in index.css).
 */
export function usePageTurn(token: string): string {
  return prefersReducedMotion() ? "static" : token;
}
