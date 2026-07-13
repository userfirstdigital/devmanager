import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import type { SessionRuntimeState } from "../api/types";
import { ControlBar } from "./ControlBar";
import { Sidebar } from "./Sidebar";

const session: SessionRuntimeState = {
  session_id: "pty-a",
  stable_session_key: "tab:a",
  pid: null,
  status: "Running",
  session_kind: "claude",
  command_id: null,
  project_id: "project-a",
  tab_id: "a",
  exit_code: null,
  title: "Claude",
  dimensions: { cols: 100, rows: 30, cell_width: 10, cell_height: 20 },
};

describe("automatic browser control", () => {
  it("does not present manual take or release control actions", () => {
    const markup = [
      renderToStaticMarkup(<ControlBar session={session} onClose={() => {}} />),
      renderToStaticMarkup(<Sidebar />),
    ].join("\n");

    expect(markup).not.toMatch(/take control|release control|view only/i);
  });
});
