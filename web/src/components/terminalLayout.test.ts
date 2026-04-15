import { describe, expect, it } from "vitest";

import {
  DESKTOP_TERMINAL_FONT_SIZE,
  MOBILE_TERMINAL_MIN_FONT_SIZE,
  computeResponsiveTerminalLayout,
} from "./terminalLayout";

describe("computeResponsiveTerminalLayout", () => {
  it("keeps the desktop font when there is enough width", () => {
    expect(
      computeResponsiveTerminalLayout({
        containerWidth: 1200,
        measuredTerminalWidth: 860,
        currentFontSize: DESKTOP_TERMINAL_FONT_SIZE,
      }),
    ).toEqual({
      fontSize: DESKTOP_TERMINAL_FONT_SIZE,
      allowHorizontalPan: false,
    });
  });

  it("shrinks the terminal font on narrow layouts before enabling pan", () => {
    expect(
      computeResponsiveTerminalLayout({
        containerWidth: 360,
        measuredTerminalWidth: 420,
        currentFontSize: DESKTOP_TERMINAL_FONT_SIZE,
      }),
    ).toEqual({
      fontSize: 11,
      allowHorizontalPan: false,
    });
  });

  it("falls back to horizontal pan after hitting the minimum font size", () => {
    expect(
      computeResponsiveTerminalLayout({
        containerWidth: 360,
        measuredTerminalWidth: 880,
        currentFontSize: DESKTOP_TERMINAL_FONT_SIZE,
      }),
    ).toEqual({
      fontSize: MOBILE_TERMINAL_MIN_FONT_SIZE,
      allowHorizontalPan: true,
    });
  });
});
