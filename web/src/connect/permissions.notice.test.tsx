import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { GuestActionNotice } from "./GuestActionNotice";
import type { CapabilityGrant } from "./permissions";

const watcher: CapabilityGrant = {
  role: "watcher",
  taskId: "tab:a",
  actions: ["readTask", "readPresence"],
};

const owner: CapabilityGrant = {
  role: "owner",
  taskId: "tab:a",
  actions: [
    "readTask",
    "readPresence",
    "mutateTask",
    "sendPrompt",
    "answerRequest",
    "approveDangerous",
    "readPersonalPrompts",
  ],
};

describe("GuestActionNotice", () => {
  it("shows a watcher denial instead of hiding the security failure", () => {
    const markup = renderToStaticMarkup(
      <GuestActionNotice grant={watcher} action="sendPrompt" />,
    );
    expect(markup).toContain("guest-action-disabled");
    expect(markup).toContain("View only");
    expect(markup).toContain("data-reason=\"watcher\"");
  });

  it("shows reconnecting when a permitted action cannot run yet", () => {
    const markup = renderToStaticMarkup(
      <GuestActionNotice
        grant={owner}
        action="sendPrompt"
        statusKind="connecting"
      />,
    );
    expect(markup).toContain("Reconnecting");
    expect(markup).toContain("data-reason=\"reconnecting\"");
  });

  it("stays silent when the owner is allowed to act", () => {
    const markup = renderToStaticMarkup(
      <GuestActionNotice grant={owner} action="sendPrompt" />,
    );
    expect(markup).toBe("");
  });
});
