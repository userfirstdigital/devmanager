import { describe, expect, it, vi } from "vitest";

import {
  buildRemoteSiteUrl,
  canOpenRemoteSite,
  openRemoteSiteInNewTab,
} from "./remoteSiteLink";

describe("remoteSiteLink", () => {
  it("derives the site URL from the current browser host and target port", () => {
    expect(
      buildRemoteSiteUrl("http://192.168.1.50:43871/remote?tab=web#hash", 3000),
    ).toBe("http://192.168.1.50:3000/");
  });

  it("only exposes the remote-site action for running servers with a known port", () => {
    expect(canOpenRemoteSite({ port: 3000 }, { status: "Running" })).toBe(true);
    expect(canOpenRemoteSite({ port: 3000 }, { status: "Starting" })).toBe(false);
    expect(canOpenRemoteSite({ port: null }, { status: "Running" })).toBe(false);
    expect(canOpenRemoteSite({ port: 3000 }, null)).toBe(false);
  });

  it("opens the derived site URL in a new tab", () => {
    const open = vi.fn();

    const url = openRemoteSiteInNewTab(
      open,
      "http://10.0.0.8:43871/projects",
      5173,
    );

    expect(url).toBe("http://10.0.0.8:5173/");
    expect(open).toHaveBeenCalledWith(
      "http://10.0.0.8:5173/",
      "_blank",
      "noopener,noreferrer",
    );
  });
});
