import { useState } from 'react';
import { X, Volume2 } from 'lucide-react';
import { useAppStore } from '../../stores/appStore';
import { ImportExport } from './ImportExport';
import type { DefaultTerminal, MacTerminalProfile } from '../../types/config';
import { NOTIFICATION_SOUNDS, playNotificationSound } from '../../utils/notificationSound';
import { isMacPlatform } from '../../utils/runtimePlatform';

export function SettingsDialog({ onClose }: { onClose: () => void }) {
  const config = useAppStore(s => s.config);
  const runtimeInfo = useAppStore(s => s.runtimeInfo);
  const updateSettings = useAppStore(s => s.updateSettings);
  const [showImportExport, setShowImportExport] = useState(false);

  if (!config) return null;
  const settings = config.settings;
  const isMac = isMacPlatform(runtimeInfo);
  const terminalValue = isMac ? (settings.macTerminalProfile || 'system') : (settings.defaultTerminal || 'bash');
  const terminalLabel = isMac ? 'Default terminal shell' : 'Default terminal';
  const terminalDescription = isMac
    ? 'Shell used for Claude Code and interactive terminals on macOS'
    : 'Shell used for Claude Code and interactive terminals';
  const systemShellLabel = runtimeInfo?.userShellName ? `User shell (${runtimeInfo.userShellName})` : 'User shell';

  const toggle = (key: 'confirmOnClose' | 'minimizeToTray' | 'restoreSessionOnStart') => {
    updateSettings({ ...settings, [key]: !settings[key] });
  };

  const handleLogBufferChange = (value: string) => {
    const num = parseInt(value, 10);
    if (!isNaN(num) && num >= 100 && num <= 100000) {
      updateSettings({ ...settings, logBufferSize: num });
    }
  };

  const handleTerminalChange = (value: string) => {
    if (isMac) {
      updateSettings({ ...settings, macTerminalProfile: value as MacTerminalProfile });
      return;
    }

    updateSettings({ ...settings, defaultTerminal: value as DefaultTerminal });
  };

  return (
    <div className="fixed inset-0 bg-black/60 flex items-center justify-center z-50" onClick={onClose}>
      <div
        className="bg-zinc-800 rounded-lg border border-zinc-700 shadow-xl w-[450px] max-h-[80vh] flex flex-col"
        onClick={e => e.stopPropagation()}
      >
        <div className="flex items-center justify-between p-4 border-b border-zinc-700">
          <h2 className="text-sm font-semibold text-zinc-100">Settings</h2>
          <button onClick={onClose} className="p-1 hover:bg-zinc-700 rounded text-zinc-400">
            <X size={16} />
          </button>
        </div>

        <div className="flex-1 overflow-y-auto p-4 space-y-4">
          <ToggleRow
            label="Confirm on close"
            description="Show confirmation dialog when closing with running servers"
            checked={settings.confirmOnClose}
            onChange={() => toggle('confirmOnClose')}
          />

          <ToggleRow
            label="Minimize to tray"
            description="Minimize to system tray instead of closing"
            checked={settings.minimizeToTray}
            onChange={() => toggle('minimizeToTray')}
          />

          <ToggleRow
            label="Resume previous session on startup"
            description="Restore open tabs and sidebar state on launch"
            checked={settings.restoreSessionOnStart !== false}
            onChange={() => toggle('restoreSessionOnStart')}
          />

          <div className="space-y-1">
            <label className="text-xs text-zinc-200 font-medium">Log buffer size</label>
            <p className="text-[10px] text-zinc-500">Maximum number of log lines to keep per command (100 - 100,000)</p>
            <input
              type="number"
              value={settings.logBufferSize}
              onChange={e => handleLogBufferChange(e.target.value)}
              min={100}
              max={100000}
              step={1000}
              className="w-32 px-3 py-1.5 bg-zinc-900 border border-zinc-700 rounded text-xs text-zinc-200 focus:outline-none focus:border-indigo-500"
            />
          </div>

          <div className="space-y-1">
            <label className="text-xs text-zinc-200 font-medium">{terminalLabel}</label>
            <p className="text-[10px] text-zinc-500">{terminalDescription}</p>
            <select
              value={terminalValue}
              onChange={e => handleTerminalChange(e.target.value)}
              className="w-48 px-3 py-1.5 bg-zinc-900 border border-zinc-700 rounded text-xs text-zinc-200 focus:outline-none focus:border-indigo-500"
            >
              {isMac ? (
                <>
                  <option value="system">{systemShellLabel}</option>
                  <option value="zsh">zsh</option>
                  <option value="bash">bash</option>
                </>
              ) : (
                <>
                  <option value="bash">Bash (Git Bash)</option>
                  <option value="powershell">PowerShell</option>
                  <option value="cmd">CMD</option>
                </>
              )}
            </select>
          </div>

          <div className="space-y-1">
            <label className="text-xs text-zinc-200 font-medium">Notification sound</label>
            <p className="text-[10px] text-zinc-500">Sound played when an AI terminal finishes a long task</p>
            <div className="flex items-center gap-2">
              <select
                value={settings.notificationSound || 'glass'}
                onChange={e => updateSettings({ ...settings, notificationSound: e.target.value })}
                className="w-40 px-3 py-1.5 bg-zinc-900 border border-zinc-700 rounded text-xs text-zinc-200 focus:outline-none focus:border-indigo-500"
              >
                {NOTIFICATION_SOUNDS.map(s => (
                  <option key={s.id} value={s.id}>{s.label}</option>
                ))}
              </select>
              <button
                onClick={() => playNotificationSound(settings.notificationSound || 'glass')}
                className="p-1.5 rounded hover:bg-zinc-700 text-zinc-400 hover:text-zinc-200"
                title="Preview sound"
              >
                <Volume2 size={14} />
              </button>
            </div>
          </div>

          <div className="space-y-1">
            <label className="text-xs text-zinc-200 font-medium">
              Terminal font size <span className="text-zinc-400 font-normal ml-1">{settings.terminalFontSize ?? 13}px</span>
            </label>
            <p className="text-[10px] text-zinc-500">Default text size for all terminals</p>
            <input
              type="range"
              min={8}
              max={24}
              step={1}
              value={settings.terminalFontSize ?? 13}
              onChange={e => updateSettings({ ...settings, terminalFontSize: parseInt(e.target.value, 10) })}
              className="w-48 h-1.5 accent-indigo-500"
            />
          </div>

          <div className="space-y-1">
            <label className="text-xs text-zinc-200 font-medium">Claude command</label>
            <p className="text-[10px] text-zinc-500">Command launched when opening a Claude terminal</p>
            <input
              type="text"
              value={settings.claudeCommand || ''}
              onChange={e => updateSettings({ ...settings, claudeCommand: e.target.value || undefined })}
              placeholder="npx -y @anthropic-ai/claude-code@latest --dangerously-skip-permissions"
              className="w-full px-3 py-1.5 bg-zinc-900 border border-zinc-700 rounded text-xs text-zinc-200 focus:outline-none focus:border-indigo-500 placeholder:text-zinc-600"
            />
          </div>

          <div className="space-y-1">
            <label className="text-xs text-zinc-200 font-medium">Codex command</label>
            <p className="text-[10px] text-zinc-500">Command launched when opening a Codex terminal</p>
            <input
              type="text"
              value={settings.codexCommand || ''}
              onChange={e => updateSettings({ ...settings, codexCommand: e.target.value || undefined })}
              placeholder="npx -y @openai/codex@latest --dangerously-bypass-approvals-and-sandbox"
              className="w-full px-3 py-1.5 bg-zinc-900 border border-zinc-700 rounded text-xs text-zinc-200 focus:outline-none focus:border-indigo-500 placeholder:text-zinc-600"
            />
          </div>

          <div className="border-t border-zinc-700 pt-4">
            <label className="text-xs text-zinc-200 font-medium block mb-2">Data</label>
            <button
              onClick={() => setShowImportExport(true)}
              className="px-4 py-1.5 bg-zinc-700 hover:bg-zinc-600 text-zinc-300 text-xs rounded"
            >
              Import / Export Configuration
            </button>
          </div>
        </div>

        <div className="flex justify-end p-4 border-t border-zinc-700">
          <button
            onClick={onClose}
            className="px-4 py-1.5 bg-indigo-600 hover:bg-indigo-500 text-white text-xs font-medium rounded"
          >
            Done
          </button>
        </div>
      </div>

      {showImportExport && <ImportExport onClose={() => setShowImportExport(false)} />}
    </div>
  );
}

function ToggleRow({
  label,
  description,
  checked,
  onChange,
}: {
  label: string;
  description: string;
  checked: boolean;
  onChange: () => void;
}) {
  return (
    <div className="flex items-start justify-between gap-4">
      <div>
        <div className="text-xs text-zinc-200 font-medium">{label}</div>
        <div className="text-[10px] text-zinc-500">{description}</div>
      </div>
      <button
        onClick={onChange}
        className={`relative w-9 h-5 rounded-full flex-shrink-0 transition-colors ${
          checked ? 'bg-indigo-600' : 'bg-zinc-600'
        }`}
      >
        <div
          className={`absolute top-0.5 w-4 h-4 rounded-full bg-white transition-transform ${
            checked ? 'translate-x-4' : 'translate-x-0.5'
          }`}
        />
      </button>
    </div>
  );
}
