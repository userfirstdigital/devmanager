import { useState, useEffect } from 'react';
import { X, Plus, Save, Loader2 } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import type { EnvEntry } from '../../types/config';

interface EnvEditorProps {
  filePath: string;
  onClose: () => void;
}

export function EnvEditor({ filePath, onClose }: EnvEditorProps) {
  const [entries, setEntries] = useState<EnvEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [editingIndex, setEditingIndex] = useState<number | null>(null);
  const [editValue, setEditValue] = useState('');
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    loadEntries();
  }, [filePath]);

  const loadEntries = async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await invoke<EnvEntry[]>('read_env_file', { filePath });
      setEntries(result);
    } catch (err) {
      setError(`Failed to read env file: ${err}`);
    }
    setLoading(false);
  };

  const handleSave = async () => {
    setSaving(true);
    setError(null);
    try {
      await invoke('write_env_file', { filePath, entries });
    } catch (err) {
      setError(`Failed to save: ${err}`);
    }
    setSaving(false);
  };

  const handleStartEdit = (index: number, value: string) => {
    setEditingIndex(index);
    setEditValue(value);
  };

  const handleFinishEdit = (index: number) => {
    const updated = [...entries];
    const entry = updated[index];
    if (entry.type === 'variable') {
      updated[index] = { ...entry, value: editValue, raw: `${entry.key}=${editValue}` };
      setEntries(updated);
    }
    setEditingIndex(null);
  };

  const handleAddVariable = () => {
    setEntries([
      ...entries,
      { type: 'variable', key: 'NEW_VAR', value: '', raw: 'NEW_VAR=' },
    ]);
  };

  const handleKeyChange = (index: number, newKey: string) => {
    const updated = [...entries];
    const entry = updated[index];
    if (entry.type === 'variable') {
      updated[index] = { ...entry, key: newKey, raw: `${newKey}=${entry.value ?? ''}` };
      setEntries(updated);
    }
  };

  const handleRemoveEntry = (index: number) => {
    setEntries(entries.filter((_, i) => i !== index));
  };

  return (
    <div className="fixed inset-0 bg-black/60 flex items-center justify-center z-50" onClick={onClose}>
      <div
        className="bg-zinc-800 rounded-lg border border-zinc-700 shadow-xl w-[600px] max-h-[80vh] flex flex-col"
        onClick={e => e.stopPropagation()}
      >
        <div className="flex items-center justify-between p-4 border-b border-zinc-700">
          <h2 className="text-sm font-semibold text-zinc-100">Environment Variables</h2>
          <div className="flex items-center gap-2">
            <span className="text-[10px] text-zinc-500 font-mono truncate max-w-[200px]">{filePath}</span>
            <button onClick={onClose} className="p-1 hover:bg-zinc-700 rounded text-zinc-400">
              <X size={16} />
            </button>
          </div>
        </div>

        <div className="flex-1 overflow-y-auto p-4">
          {loading ? (
            <div className="flex items-center justify-center py-8 gap-2 text-zinc-400 text-xs">
              <Loader2 size={14} className="animate-spin" />
              Loading...
            </div>
          ) : error ? (
            <div className="text-xs text-red-400 bg-red-400/10 px-3 py-2 rounded">{error}</div>
          ) : (
            <table className="w-full text-xs">
              <thead>
                <tr className="text-zinc-500 border-b border-zinc-700">
                  <th className="text-left py-1.5 px-2 w-1/3">Key</th>
                  <th className="text-left py-1.5 px-2">Value</th>
                  <th className="w-10" />
                </tr>
              </thead>
              <tbody>
                {entries.map((entry, index) => {
                  if (entry.type === 'blank') {
                    return (
                      <tr key={index} className="h-6">
                        <td colSpan={3} />
                      </tr>
                    );
                  }

                  if (entry.type === 'comment') {
                    return (
                      <tr key={index}>
                        <td colSpan={3} className="py-1 px-2 text-zinc-500 italic font-mono">
                          {entry.raw}
                        </td>
                      </tr>
                    );
                  }

                  return (
                    <tr key={index} className="border-b border-zinc-700/50 hover:bg-zinc-700/30">
                      <td className="py-1 px-2">
                        <input
                          type="text"
                          value={entry.key ?? ''}
                          onChange={e => handleKeyChange(index, e.target.value)}
                          className="bg-transparent text-zinc-200 font-mono w-full focus:outline-none focus:bg-zinc-900 rounded px-1"
                        />
                      </td>
                      <td className="py-1 px-2">
                        {editingIndex === index ? (
                          <input
                            type="text"
                            value={editValue}
                            onChange={e => setEditValue(e.target.value)}
                            onBlur={() => handleFinishEdit(index)}
                            onKeyDown={e => { if (e.key === 'Enter') handleFinishEdit(index); }}
                            className="bg-zinc-900 border border-zinc-600 text-zinc-200 font-mono w-full rounded px-1 focus:outline-none focus:border-indigo-500"
                            autoFocus
                          />
                        ) : (
                          <span
                            className="text-zinc-300 font-mono cursor-pointer hover:text-zinc-100 block px-1"
                            onClick={() => handleStartEdit(index, entry.value ?? '')}
                          >
                            {entry.value || <span className="text-zinc-600 italic">empty</span>}
                          </span>
                        )}
                      </td>
                      <td className="py-1 px-1">
                        <button
                          onClick={() => handleRemoveEntry(index)}
                          className="p-0.5 rounded hover:bg-zinc-600 text-zinc-500 hover:text-red-400"
                        >
                          <X size={12} />
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          )}
        </div>

        <div className="flex items-center justify-between p-4 border-t border-zinc-700">
          <button
            onClick={handleAddVariable}
            className="flex items-center gap-1.5 px-3 py-1.5 bg-zinc-700 hover:bg-zinc-600 text-zinc-300 text-xs rounded"
          >
            <Plus size={14} />
            Add Variable
          </button>
          <div className="flex gap-2">
            <button
              onClick={onClose}
              className="px-4 py-1.5 text-xs text-zinc-400 hover:text-zinc-100"
            >
              Cancel
            </button>
            <button
              onClick={handleSave}
              disabled={saving}
              className="flex items-center gap-1.5 px-4 py-1.5 bg-indigo-600 hover:bg-indigo-500 disabled:opacity-50 text-white text-xs font-medium rounded"
            >
              {saving ? <Loader2 size={14} className="animate-spin" /> : <Save size={14} />}
              Save
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
