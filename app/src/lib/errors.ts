// Error-payload → banner mapper. Engine failures arrive as a plain-language
// `ApiError`; the UI never shows a raw error object or JSON. When a mutation was
// rolled back we append the reassuring "Everything was rolled back." sentence
// (docs/m4-spec.md §"Empty/degraded states").

import type { ApiError } from "../types";

export interface Banner {
  title: string;
  body: string;
  rolledBack: boolean;
}

export function errorBanner(e: ApiError): Banner {
  const rolled = e.rolledBack ? " Everything was rolled back." : "";
  return {
    title: e.title,
    body: `${e.detail}${rolled}`.trim(),
    rolledBack: e.rolledBack,
  };
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
