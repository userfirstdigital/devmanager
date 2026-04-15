import { Eye, KeyRound, Terminal } from "lucide-react";
import { useStore } from "../store";
import { ProjectTree } from "./ProjectTree";

interface SidebarProps {
  onItemPicked?: () => void;
}

export function shouldCloseSidebarAfterClick(
  target: Pick<HTMLElement, "closest">,
): boolean {
  if (target.closest("[data-sidebar-action='true']")) {
    return false;
  }
  return Boolean(target.closest("[data-sidebar-row='true']"));
}

function ControlToggle() {
  const youHaveControl = useStore(
    (s) => s.snapshot?.youHaveControl ?? false,
  );
  const takeControl = useStore((s) => s.takeControl);
  const releaseControl = useStore((s) => s.releaseControl);

  if (youHaveControl) {
    return (
      <button
        type="button"
        onClick={releaseControl}
        title="Release control so the desktop app can type again"
        className="flex items-center gap-1.5 text-[11px] px-2 py-1 rounded bg-emerald-600/20 text-emerald-300 hover:bg-emerald-600/30"
      >
        <KeyRound className="size-3" />
        <span>Control</span>
      </button>
    );
  }
  return (
    <button
      type="button"
      onClick={takeControl}
      title="Take control so this browser can start servers and type"
      className="flex items-center gap-1.5 text-[11px] px-2 py-1 rounded bg-amber-600/20 text-amber-300 hover:bg-amber-600/30"
    >
      <Eye className="size-3" />
      <span>View</span>
    </button>
  );
}

export function Sidebar({ onItemPicked }: SidebarProps) {
  return (
    <aside
      className="w-60 shrink-0 bg-zinc-800 border-r border-zinc-700 flex flex-col h-full"
      onClick={(e) => {
        // Only close the mobile drawer for actual row picks. Nested controls
        // like start/stop/restart must stay interactive on touch devices.
        const target = e.target as HTMLElement;
        if (shouldCloseSidebarAfterClick(target)) onItemPicked?.();
      }}
    >
      <header className="flex items-center gap-2 px-3 h-11 shrink-0 border-b border-zinc-700">
        <Terminal className="size-4 text-indigo-400" />
        <span className="text-sm font-semibold text-zinc-100 flex-1">
          DevManager
        </span>
        <ControlToggle />
      </header>
      <div className="flex-1 overflow-y-auto py-2">
        <ProjectTree />
      </div>
    </aside>
  );
}
