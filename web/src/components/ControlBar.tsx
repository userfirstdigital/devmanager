import { Eye, KeyRound, X } from "lucide-react";
import { useStore } from "../store";
import type { SessionRuntimeState } from "../api/types";

interface ControlBarProps {
  session: SessionRuntimeState;
  onClose: () => void;
}

/**
 * Thin header strip above the terminal: session label, control-state
 * indicator, take/release button, close button. Rendered inside the main
 * content area only when a session is active.
 */
export function ControlBar({ session, onClose }: ControlBarProps) {
  const youHaveControl = useStore(
    (s) => s.snapshot?.youHaveControl ?? false,
  );
  const takeControl = useStore((s) => s.takeControl);
  const releaseControl = useStore((s) => s.releaseControl);

  return (
    <div className="flex items-center gap-2 px-2 md:px-3 h-8 md:h-9 shrink-0 bg-zinc-800/60 border-b border-zinc-700/60">
      <span className="text-xs font-medium text-zinc-200 truncate">
        {session.title || session.session_id}
      </span>
      <span className="text-[10px] text-zinc-500 shrink-0">
        {session.status}
      </span>
      <div className="flex-1" />
      {youHaveControl ? (
        <button
          type="button"
          onClick={releaseControl}
          className="flex items-center gap-1.5 text-[10px] md:text-[11px] px-2 py-1 rounded bg-emerald-600/20 text-emerald-300 hover:bg-emerald-600/30"
        >
          <KeyRound className="size-3" />
          <span>You have control</span>
        </button>
      ) : (
        <button
          type="button"
          onClick={takeControl}
          className="flex items-center gap-1.5 text-[10px] md:text-[11px] px-2 py-1 rounded bg-amber-600/20 text-amber-300 hover:bg-amber-600/30"
        >
          <Eye className="size-3" />
          <span>View only — take control</span>
        </button>
      )}
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
