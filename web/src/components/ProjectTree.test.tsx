import { describe, expect, it } from "vitest";

import type { Project, ProjectFolder } from "../api/types";
import {
  describeFolderPresentation,
  sortProjectsForSidebar,
} from "./sidebarModel";

describe("sidebarModel", () => {
  it("uses the folder name as the primary label for single-command folders", () => {
    const folder: ProjectFolder = {
      id: "folder-1",
      name: "folder-alpha",
      commands: [
        {
          id: "command-1",
          label: "dev server",
          port: null,
          status: "Stopped",
        },
      ],
    };

    expect(describeFolderPresentation(folder)).toEqual({
      kind: "single-command",
      primaryLabel: "folder-alpha",
      secondaryLabel: "dev server",
    });
  });

  it("sorts pinned projects before unpinned projects", () => {
    const projects: Project[] = [
      {
        id: "project-unpinned",
        name: "Unpinned Project",
        folders: [],
        color: null,
        pinned: false,
      },
      {
        id: "project-pinned",
        name: "Pinned Project",
        folders: [],
        color: "#6366f1",
        pinned: true,
      },
    ];

    expect(sortProjectsForSidebar(projects).map((project) => project.id)).toEqual([
      "project-pinned",
      "project-unpinned",
    ]);
  });
});
