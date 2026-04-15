import { describe, expect, it } from "vitest";

import {
  MOBILE_KEY_LAYOUT,
  pickMobileKeysForWidth,
} from "./mobileKeyLayout";

describe("MOBILE_KEY_LAYOUT", () => {
  it("includes an enter shortcut for submitting commands on phones", () => {
    expect(MOBILE_KEY_LAYOUT).toContainEqual({
      label: "Enter",
      payload: "\r",
    });
  });

  it("drops the low-priority left-arrow shortcut from the primary mobile row", () => {
    expect(MOBILE_KEY_LAYOUT.map((key) => key.label)).not.toContain("Left");
  });

  it("trims low-priority keys on narrow phone widths", () => {
    const labels = pickMobileKeysForWidth(360).map((key) => key.label);

    expect(labels).toEqual([
      "Esc",
      "Tab",
      "Ctrl",
      "C",
      "D",
      "Enter",
      "Up",
      "Down",
    ]);
  });

  it("keeps the full helper row when enough width is available", () => {
    expect(pickMobileKeysForWidth(520).map((key) => key.label)).toEqual(
      MOBILE_KEY_LAYOUT.map((key) => key.label),
    );
  });
});
