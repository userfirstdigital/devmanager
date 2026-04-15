import type { Project, ProjectFolder, SSHConnection } from "../api/types";

export interface FolderPresentation {
  kind: "empty" | "single-command" | "multi-command";
  primaryLabel: string;
  secondaryLabel?: string;
}

export interface SidebarShellSummary {
  showSshSection: boolean;
  showFooterActions: boolean;
}

export function groupProjectsForSidebar(projects: Project[]): {
  pinned: Project[];
  standard: Project[];
} {
  const pinned = projects.filter((project) => project.pinned);
  const standard = projects.filter((project) => !project.pinned);
  return { pinned, standard };
}

export function sortProjectsForSidebar(projects: Project[]): Project[] {
  const { pinned, standard } = groupProjectsForSidebar(projects);
  return [...pinned, ...standard];
}

export function describeFolderPresentation(
  folder: ProjectFolder,
): FolderPresentation {
  if (folder.commands.length === 0) {
    return {
      kind: "empty",
      primaryLabel: folder.name,
    };
  }

  if (folder.commands.length === 1) {
    return {
      kind: "single-command",
      primaryLabel: folder.name,
      secondaryLabel: folder.commands[0]?.label,
    };
  }

  return {
    kind: "multi-command",
    primaryLabel: folder.name,
    secondaryLabel: `${folder.commands.length} commands`,
  };
}

export function summarizeSidebarShell(
  sshConnections: SSHConnection[],
): SidebarShellSummary {
  return {
    showSshSection: sshConnections.length > 0,
    showFooterActions: true,
  };
}

export function formatSshTarget(connection: SSHConnection): string {
  return `${connection.username}@${connection.host}:${connection.port}`;
}
