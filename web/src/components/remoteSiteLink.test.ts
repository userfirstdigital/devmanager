import { describe, expect, it, vi } from "vitest";
import type { WebPortAuthority } from "../api/types";

import {
  buildRemoteSiteUrl,
  canOpenRemoteSite,
  openRemoteSiteInNewTab,
} from "./remoteSiteLink";

describe("remoteSiteLink", () => {
  const authority = (
    overrides: Partial<
      Pick<
        WebPortAuthority,
        | "kind"
        | "controlReason"
        | "fresh"
        | "listeners"
        | "reapIncomplete"
        | "error"
        | "diagnostic"
        | "sessionId"
      >
    > = {},
  ): Pick<
    WebPortAuthority,
    | "kind"
    | "controlReason"
    | "fresh"
    | "listeners"
    | "reapIncomplete"
    | "error"
    | "diagnostic"
    | "sessionId"
  > => ({
    kind: "managed",
    controlReason: "exactManagedFence",
    fresh: true,
    sessionId: "session-1",
    reapIncomplete: false,
    error: null,
    diagnostic: null,
    listeners: [
      { pid: 42, creationTime100ns: 123, executableProven: true },
    ],
    ...overrides,
  });

  it("derives the site URL from the current browser host and target port", () => {
    expect(
      buildRemoteSiteUrl("http://192.168.1.50:43871/remote?tab=web#hash", 3000),
    ).toBe("http://192.168.1.50:3000/");
  });

  it("only exposes the remote-site action for fresh typed authority", () => {
    expect(
      canOpenRemoteSite(
        { port: 3000 },
        { status: "Running", session_id: "session-1" },
        authority(),
      ),
    ).toBe(true);
    expect(
      canOpenRemoteSite(
        { port: 3000 },
        { status: "Running", session_id: "session-1" },
        authority({ listeners: [] }),
      ),
    ).toBe(false);
    expect(
      canOpenRemoteSite(
        { port: 3000 },
        { status: "Running", session_id: "session-1" },
        authority({ error: "listener probe failed" }),
      ),
    ).toBe(false);
    expect(
      canOpenRemoteSite(
        { port: 3000 },
        { status: "Running", session_id: "session-1" },
        authority({ kind: "provenExternal", diagnostic: "probeError" }),
      ),
    ).toBe(false);
    expect(
      canOpenRemoteSite(
        { port: 3000 },
        { status: "Running", session_id: "session-1" },
        authority({ kind: "unknown", controlReason: "mixedOrUnverified" }),
      ),
    ).toBe(false);
    expect(
      canOpenRemoteSite(
        { port: 3000 },
        { status: "Running", session_id: "session-1" },
        authority({ fresh: false, controlReason: "stale" }),
      ),
    ).toBe(false);
    expect(
      canOpenRemoteSite(
        { port: 3000 },
        { status: "Running", session_id: "session-1" },
        authority({ kind: "managed", controlReason: "starting" }),
      ),
    ).toBe(false);
    expect(
      canOpenRemoteSite(
        { port: 3000 },
        { status: "Running", session_id: "session-1" },
        authority({ kind: "managedUnready", controlReason: "managedUnready" }),
      ),
    ).toBe(false);
    expect(
      canOpenRemoteSite(
        { port: 3000 },
        { status: "Running", session_id: "session-1" },
        authority({ kind: "provenExternal", controlReason: "provenExternalNoControl" }),
      ),
    ).toBe(true);
    expect(
      canOpenRemoteSite(
        { port: 3000 },
        { status: "Running", session_id: "session-1" },
        authority({ reapIncomplete: true }),
      ),
    ).toBe(false);
    expect(
      canOpenRemoteSite({ port: null }, { status: "Running", session_id: "session-1" }, authority()),
    ).toBe(false);
    expect(canOpenRemoteSite({ port: 3000 }, null, authority())).toBe(false);
    expect(
      canOpenRemoteSite(
        { port: 3000 },
        { status: "Running", session_id: "session-1" },
        authority({ reapIncomplete: undefined }),
      ),
    ).toBe(false);
    expect(
      canOpenRemoteSite(
        { port: 3000 },
        { status: "Running", session_id: "session-2" },
        authority(),
      ),
    ).toBe(false);
    expect(
      canOpenRemoteSite(
        { port: 3000 },
        { status: "Running", session_id: "session-1" },
        undefined,
      ),
    ).toBe(false);
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
