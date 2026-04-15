import { describe, expect, it } from "vitest";

import type { SSHConnection } from "../api/types";
import { summarizeSidebarShell } from "./sidebarModel";

describe("Sidebar shell model", () => {
  it("shows the SSH section when connections exist and keeps footer actions available", () => {
    const sshConnections: SSHConnection[] = [
      {
        id: "ssh-1",
        label: "Prod SSH",
        host: "example.com",
        port: 22,
        username: "deploy",
        password: null,
      },
    ];

    expect(summarizeSidebarShell(sshConnections)).toEqual({
      showSshSection: true,
      showFooterActions: true,
    });
  });
});
