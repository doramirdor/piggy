// Number/token formatting helpers, shared by the UI and the share-card renderer.

/** Insert thousands separators (e.g. 1234567 → "1,234,567"). */
export function commafy(n: number): string {
  return Math.round(n).toLocaleString("en-US");
}

/**
 * Compact token count: 2_000_000_000 → "2B", 1_200_000 → "1.2M", 184_000 →
 * "184k", 920 → "920". Keeps one decimal only when it is non-zero.
 *
 * Billions are not hypothetical: a real week came to 2,226,197,935 tokens and
 * rendered as "2007.1M", which is four digits of magnitude the reader has to
 * decode themselves.
 */
export function formatTokens(n: number): string {
  if (n >= 1_000_000_000) {
    const b = n / 1_000_000_000;
    return `${b % 1 === 0 ? b.toFixed(0) : b.toFixed(1)}B`;
  }
  if (n >= 1_000_000) {
    const m = n / 1_000_000;
    return `${m % 1 === 0 ? m.toFixed(0) : m.toFixed(1)}M`;
  }
  if (n >= 1_000) {
    return `${Math.round(n / 1000)}k`;
  }
  return String(Math.round(n));
}

/** A fraction like -0.22 → "22%" (magnitude only). */
export function pctMagnitude(fraction: number): string {
  return `${Math.round(Math.abs(fraction) * 100)}%`;
}
