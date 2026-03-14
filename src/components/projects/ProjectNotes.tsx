import { useState, useEffect, useRef } from 'react';
import { X } from 'lucide-react';
import { useAppStore } from '../../stores/appStore';
import type { Project } from '../../types/config';

interface ProjectNotesProps {
  project: Project;
  onClose: () => void;
}

export function ProjectNotes({ project, onClose }: ProjectNotesProps) {
  const [notes, setNotes] = useState(project.notes ?? '');
  const updateProject = useAppStore(s => s.updateProject);
  const saveTimeoutRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  useEffect(() => {
    setNotes(project.notes ?? '');
  }, [project.id]);

  useEffect(() => {
    if (saveTimeoutRef.current) clearTimeout(saveTimeoutRef.current);
    saveTimeoutRef.current = setTimeout(() => {
      if (notes !== (project.notes ?? '')) {
        updateProject({ ...project, notes, updatedAt: new Date().toISOString() });
      }
    }, 1000);
    return () => {
      if (saveTimeoutRef.current) clearTimeout(saveTimeoutRef.current);
    };
  }, [notes]);

  return (
    <div className="fixed inset-0 bg-black/60 flex items-center justify-center z-50" onClick={onClose}>
      <div
        className="bg-zinc-800 rounded-lg border border-zinc-700 shadow-xl w-[500px] max-h-[80vh] flex flex-col"
        onClick={e => e.stopPropagation()}
      >
        <div className="flex items-center justify-between p-4 border-b border-zinc-700">
          <h2 className="text-sm font-semibold text-zinc-100">Notes - {project.name}</h2>
          <button onClick={onClose} className="p-1 hover:bg-zinc-700 rounded text-zinc-400">
            <X size={16} />
          </button>
        </div>

        <div className="p-4">
          <textarea
            value={notes}
            onChange={e => setNotes(e.target.value)}
            placeholder="Add notes for this project..."
            className="w-full h-64 bg-zinc-900 border border-zinc-700 rounded-lg p-3 text-xs text-zinc-200 resize-none focus:outline-none focus:border-indigo-500 placeholder-zinc-600"
          />
          <p className="text-[10px] text-zinc-500 mt-1">Auto-saves after 1 second</p>
        </div>
      </div>
    </div>
  );
}
