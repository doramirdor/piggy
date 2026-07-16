// Error-payload → banner mapper. Engine failures arrive as a plain-language
// `ApiError`; the UI never shows a raw error object or JSON. When a mutation was
// rolled back we append the reassuring "Everything was rolled back." sentence
// (docs/m4-spec.md §"Empty/degraded states").

import type { ApiError } from "../types";

export interface Banner {
  title: string;
  body: string;
  rolledBack: boolean;
  /** "error" is the red alert banner; "info" is a neutral heads-up (e.g. a
   * conflicting saver was auto-disabled). Defaults to "error". */
  kind: "error" | "info";
}

export function errorBanner(e: ApiError): Banner {
  const rolled = e.rolledBack ? " Everything was rolled back." : "";
  return {
    title: e.title,
    body: `${e.detail}${rolled}`.trim(),
    rolledBack: e.rolledBack,
    kind: "error",
  };
}

/** A neutral, dismissible heads-up - no title, just the one-line message. */
export function infoBanner(body: string): Banner {
  return { title: "", body, rolledBack: false, kind: "info" };
}

/** Normalize any thrown/rejected value into an `ApiError`. */
export function toApiError(e: unknown): ApiError {
  if (
    e &&
    typeof e === "object" &&
    "title" in e &&
    "detail" in e &&
    typeof (e as Record<string, unknown>).title === "string"
  ) {
    const o = e as Record<string, unknown>;
    return {
      title: String(o.title),
      detail: String(o.detail ?? ""),
      rolledBack: Boolean(o.rolledBack),
    };
  }
  return {
    title: "Something went wrong",
    detail: typeof e === "string" ? e : String((e as Error)?.message ?? e),
    rolledBack: false,
  };
}
