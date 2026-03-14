import { invoke } from '@tauri-apps/api/core';
import type { ScanResult, DependencyStatus, PortStatus, PortConflict, EnvEntry } from '../types/config';

export function useConfig() {
  const scanProject = async (folderPath: string): Promise<ScanResult> => {
    return invoke<ScanResult>('scan_project', { folderPath });
  };

  const checkDependencies = async (folderPath: string): Promise<DependencyStatus> => {
    return invoke<DependencyStatus>('check_dependencies', { folderPath });
  };

  const getGitBranch = async (folderPath: string): Promise<string | null> => {
    return invoke<string | null>('get_git_branch', { folderPath });
  };

  const checkPortInUse = async (port: number): Promise<PortStatus> => {
    return invoke<PortStatus>('check_port_in_use', { port });
  };

  const killPort = async (port: number): Promise<void> => {
    return invoke('kill_port', { port });
  };

  const getPortConflicts = async (): Promise<PortConflict[]> => {
    return invoke<PortConflict[]>('get_port_conflicts');
  };

  const updateEnvPort = async (envFilePath: string, variable: string, newPort: number): Promise<void> => {
    return invoke('update_env_port', { envFilePath, variable, newPort });
  };

  const readEnvFile = async (filePath: string): Promise<EnvEntry[]> => {
    return invoke<EnvEntry[]>('read_env_file', { filePath });
  };

  const writeEnvFile = async (filePath: string, entries: EnvEntry[]): Promise<void> => {
    return invoke('write_env_file', { filePath, entries });
  };

  const openTerminal = async (folderPath: string): Promise<void> => {
    return invoke('open_terminal', { folderPath });
  };

  return {
    scanProject,
    checkDependencies,
    getGitBranch,
    checkPortInUse,
    killPort,
    getPortConflicts,
    updateEnvPort,
    readEnvFile,
    writeEnvFile,
    openTerminal,
  };
}
