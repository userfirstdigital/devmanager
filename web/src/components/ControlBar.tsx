import { X } from "lucide-react";
import type { SessionRuntimeState } from "../api/types";

interface ControlBarProps {
  session: SessionRuntimeState;
  onClose: () => void;
}

/**
 * Thin header strip above the terminal. Writer ownership follows interaction,
 * so there is intentionally no manual take/release control affordance.
 */
export function ControlBar({ session, onClose }: ControlBarProps) {
  return (
    <div className="flex items-center gap-2 px-2 md:px-3 h-8 md:h-9 shrink-0 bg-zinc-800/60 border-b border-zinc-700/60">
      <span className="text-xs font-medium text-zinc-200 truncate">
        {session.title || session.session_id}
      </span>
      <span className="text-[10px] text-zinc-500 shrink-0">
        {session.status}
      </span>
      <div className="flex-1" />
      <button
        type="button"
        onClick={onClose}
        aria-label="Close terminal"
        className="p-1 rounded hover:bg-zinc-700 text-zinc-400 hover:text-zinc-100"
      >
        <X className="size-4" />
      </button>
    </div>
  );
}
