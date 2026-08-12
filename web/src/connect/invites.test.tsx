import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { CollaborationPanel } from "./invites";
import { inviteIsLive } from "./invites";

describe("invites ui", () => {
  it("renders nothing until the owner creates an invite", () => {
    expect(renderToStaticMarkup(<CollaborationPanel invites={[]} />)).toBe("");
  });

  it("lists task-scoped guests after an invite exists", () => {
    const html = renderToStaticMarkup(
      <CollaborationPanel
        invites={[
          {
            inviteId: "inv-1",
            taskId: "task-1",
            nickname: "reviewer",
            role: "watcher",
            usePolicy: "singleUse",
            expiresAtMs: 100,
            revoked: false,
          },
        ]}
      />,
    );
    expect(html).toContain("reviewer");
    expect(html).toContain("watcher");
    expect(html).not.toContain("PAIRCODE");
    expect(
      inviteIsLive(
        {
          inviteId: "inv-1",
          taskId: "task-1",
          nickname: "reviewer",
          role: "watcher",
          usePolicy: "singleUse",
          expiresAtMs: 50,
          revoked: true,
        },
        10,
      ),
    ).toBe(false);
  });
});
