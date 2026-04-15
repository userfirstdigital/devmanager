import { describe, expect, it } from "vitest";
import { shouldCloseSidebarAfterClick } from "./Sidebar";

function makeTarget(matches: Record<string, boolean>): Pick<HTMLElement, "closest"> {
  return {
    closest(selector: string) {
      return matches[selector] ? ({} as Element) : null;
    },
  };
}

describe("shouldCloseSidebarAfterClick", () => {
  it("does not close the mobile drawer for nested action buttons", () => {
    const target = makeTarget({
      "[data-sidebar-action='true']": true,
      "[data-sidebar-row='true']": true,
    });

    expect(shouldCloseSidebarAfterClick(target)).toBe(false);
  });

  it("closes the mobile drawer for row picks", () => {
    const target = makeTarget({
      "[data-sidebar-row='true']": true,
    });

    expect(shouldCloseSidebarAfterClick(target)).toBe(true);
  });
});
