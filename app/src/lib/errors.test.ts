import { describe, it, expect } from "vitest";
import { errorBanner, toApiError } from "./errors";
import type { ApiError } from "../types";

describe("errorBanner", () => {
  it("maps a plain error to a title + body banner", () => {
    const e: ApiError = {
      title: "Couldn't turn that saver on",
      detail: "The download couldn't be verified.",
      rolledBack: false,
    };
    const b = errorBanner(e);
    expect(b.title).toBe("Couldn't turn that saver on");
    expect(b.body).toBe("The download couldn't be verified.");
    expect(b.rolledBack).toBe(false);
  });

  it("appends the reassurance sentence when the change was rolled back", () => {
    const e: ApiError = {
      title: "That saver couldn't be turned on",
      detail: "It failed its health check.",
      rolledBack: true,
    };
    const b = errorBanner(e);
    expect(b.body).toBe("It failed its health check. Everything was rolled back.");
    expect(b.rolledBack).toBe(true);
  });
});

describe("toApiError", () => {
  it("passes through a well-formed ApiError", () => {
    const e = { title: "Nope", detail: "bad", rolledBack: true };
    expect(toApiError(e)).toEqual(e);
  });

  it("coerces rolledBack to a boolean", () => {
    const e = toApiError({ title: "X", detail: "y" });
    expect(e.rolledBack).toBe(false);
  });

  it("wraps a raw string", () => {
    const e = toApiError("boom");
    expect(e.title).toBe("Something went wrong");
    expect(e.detail).toBe("boom");
    expect(e.rolledBack).toBe(false);
  });

  it("wraps an Error object using its message", () => {
    const e = toApiError(new Error("kaboom"));
    expect(e.detail).toBe("kaboom");
  });
});
