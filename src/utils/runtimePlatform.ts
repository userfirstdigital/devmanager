import type {
  MacTerminalProfile,
  RunCommand,
  RuntimePlatformInfo,
  Settings,
} from '../types/config';

export interface ResolvedShellCommand {
  command: string;
  args: string[];
}

const LEGACY_GIT_BASH_PATH = 'C:/Program Files/Git/bin/bash.exe';

function getMacShellPath(
  runtimeInfo: RuntimePlatformInfo | null,
  profile: MacTerminalProfile | undefined,
): string {
  switch (profile ?? 'system') {
    case 'zsh':
      return '/bin/zsh';
    case 'bash':
      return '/bin/bash';
    case 'system':
    default:
      return runtimeInfo?.userShellPath || '/bin/zsh';
  }
}

export function isMacPlatform(runtimeInfo: RuntimePlatformInfo | null): boolean {
  return runtimeInfo?.os === 'macos';
}

export function resolveExternalTerminalShellPath(
  runtimeInfo: RuntimePlatformInfo | null,
  settings: Settings,
): string | null {
  return isMacPlatform(runtimeInfo)
    ? getMacShellPath(runtimeInfo, settings.macTerminalProfile)
    : null;
}

export function resolveInteractiveShellCommand(
  runtimeInfo: RuntimePlatformInfo | null,
  settings: Settings,
): ResolvedShellCommand {
  if (isMacPlatform(runtimeInfo)) {
    return {
      command: getMacShellPath(runtimeInfo, settings.macTerminalProfile),
      args: ['-l'],
    };
  }

  switch (settings.defaultTerminal) {
    case 'powershell':
      return { command: 'powershell.exe', args: [] };
    case 'cmd':
      return { command: 'cmd.exe', args: [] };
    case 'bash':
    default:
      return {
        command: runtimeInfo?.gitBashPath || LEGACY_GIT_BASH_PATH,
        args: ['--login'],
      };
  }
}

export function buildServerLaunchCommand(
  runtimeInfo: RuntimePlatformInfo | null,
  settings: Settings,
  runCommand: RunCommand,
): ResolvedShellCommand {
  if (!isMacPlatform(runtimeInfo)) {
    return {
      command: 'cmd',
      args: ['/C', runCommand.command, ...runCommand.args],
    };
  }

  return {
    command: getMacShellPath(runtimeInfo, settings.macTerminalProfile),
    args: ['-l', '-c', buildShellCommandLine(runCommand.command, runCommand.args)],
  };
}

function buildShellCommandLine(command: string, args: string[]): string {
  const parts = [command.trim(), ...args.map(shellQuote)];
  return parts.filter(Boolean).join(' ');
}

function shellQuote(value: string): string {
  if (value === '') {
    return "''";
  }
  return `'${value.replace(/'/g, `'\"'\"'`)}'`;
}
