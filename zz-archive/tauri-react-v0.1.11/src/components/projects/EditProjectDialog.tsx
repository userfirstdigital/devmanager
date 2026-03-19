import { useState } from 'react';
import { X } from 'lucide-react';
import { useAppStore } from '../../stores/appStore';
import type { Project } from '../../types/config';

const PALETTE = ['#6366f1', '#ec4899', '#f59e0b', '#10b981', '#3b82f6', '#ef4444', '#8b5cf6', '#14b8a6'];

export function EditProjectDialog({ project, onClose }: { project: Project; onClose: () => void }) {
  const [name, setName] = useState(project.name);
  const [color, setColor] = useState(project.color || PALETTE[0]);
  const updateProject = useAppStore(s => s.updateProject);

  const handleSave = async () => {
    if (!name.trim()) return;
    await updateProject({
      ...project,
      name: name.trim(),
      color,
      updatedAt: new Date().toISOString(),
    });
    onClose();
  };

  return (
    <div className="fixed inset-0 bg-black/60 flex items-center justify-center z-50" onClick={onClose}>
      <div className="bg-zinc-800 rounded-lg border border-zinc-700 shadow-xl w-[400px]" onClick={e => e.stopPropagation()}>
        <div className="flex items-center justify-between p-4 border-b border-zinc-700">
          <h2 className="text-sm font-semibold text-zinc-100">Edit Project</h2>
          <button onClick={onClose} className="p-1 hover:bg-zinc-700 rounded text-zinc-400">
            <X size={16} />
          </button>
        </div>

        <div className="p-4 space-y-4">
          <div>
            <label className="text-xs text-zinc-400 mb-1 block">Project Name</label>
            <input
              type="text"
              value={name}
              onChange={e => setName(e.target.value)}
              className="w-full px-3 py-2 bg-zinc-900 border border-zinc-700 rounded text-xs text-zinc-100 focus:outline-none focus:border-indigo-500"
              autoFocus
              onKeyDown={e => { if (e.key === 'Enter') handleSave(); }}
            />
          </div>

          <div>
            <label className="text-xs text-zinc-400 mb-1 block">Color</label>
            <div className="flex gap-2">
              {PALETTE.map(c => (
                <button
                  key={c}
                  onClick={() => setColor(c)}
                  className={`w-6 h-6 rounded-full border-2 ${color === c ? 'border-white' : 'border-transparent'}`}
                  style={{ backgroundColor: c }}
                />
              ))}
            </div>
          </div>
        </div>

        <div className="flex justify-end gap-2 p-4 border-t border-zinc-700">
          <button onClick={onClose} className="px-4 py-1.5 text-xs text-zinc-400 hover:text-zinc-100">
            Cancel
          </button>
          <button
            onClick={handleSave}
            disabled={!name.trim()}
            className="px-4 py-1.5 bg-indigo-600 hover:bg-indigo-500 disabled:opacity-50 disabled:cursor-not-allowed text-white text-xs font-medium rounded"
          >
            Save
          </button>
        </div>
      </div>
    </div>
  );
}
