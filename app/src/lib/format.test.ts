import { describe, it, expect } from "vitest";
import { formatTokens, commafy } from "./format";

describe("formatTokens", () => {
  it("scales into billions instead of four-digit millions", () => {
    // A real week came to 2,226,197,935 tokens and rendered as "2007.1M",
    // which makes the reader count digits to work out the magnitude.
    expect(formatTokens(2_226_197_935)).toBe("2.2B");
    expect(formatTokens(2_000_000_000)).toBe("2B");
  });

  it("keeps the existing millions/thousands behaviour", () => {
    expect(formatTokens(950_000_000)).toBe("950M");
    expect(formatTokens(1_200_000)).toBe("1.2M");
    expect(formatTokens(184_000)).toBe("184k");
    expect(formatTokens(920)).toBe("920");
  });

  it("commafy is unchanged", () => {
    expect(commafy(2_226_197_935)).toBe("2,226,197,935");
  });
});
