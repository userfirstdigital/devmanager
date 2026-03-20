import type { Project, ProjectFolder, RunCommand } from '../types/config';

export function findFolderForCommand(project: Project, commandId: string): ProjectFolder | undefined {
  return project.folders.find(f => f.commands.some(c => c.id === commandId));
}

export function getAllCommands(project: Project): RunCommand[] {
  return project.folders.flatMap(f => f.commands);
}
