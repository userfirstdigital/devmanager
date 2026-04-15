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
      folderPath: "C:\\Code\\project\\folder-alpha",
      commands: [
        {
          id: "command-1",
          label: "dev server",
          command: "npm",
          args: ["run", "dev"],
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
        rootPath: "C:\\Code\\unpinned",
        folders: [],
        color: null,
        pinned: false,
        notes: null,
        createdAt: "2026-01-01T00:00:00.000Z",
        updatedAt: "2026-01-01T00:00:00.000Z",
      },
      {
        id: "project-pinned",
        name: "Pinned Project",
        rootPath: "C:\\Code\\pinned",
        folders: [],
        color: "#6366f1",
        pinned: true,
        notes: "important",
        createdAt: "2026-01-01T00:00:00.000Z",
        updatedAt: "2026-01-01T00:00:00.000Z",
      },
    ];

    expect(sortProjectsForSidebar(projects).map((project) => project.id)).toEqual([
      "project-pinned",
      "project-unpinned",
    ]);
  });
});
