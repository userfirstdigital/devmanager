import { Activity, Server, Wifi, WifiOff } from "lucide-react";
import { useEffect, useState } from "react";
import { useStore } from "../store";
import { isLiveStatus } from "../api/types";

function useClock(): string {
  const [now, setNow] = useState(() => new Date());
  useEffect(() => {
    const id = setInterval(() => setNow(new Date()), 30_000);
    return () => clearInterval(id);
  }, []);
  return now.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

export function StatusBar() {
  const status = useStore((s) => s.status);
  const snapshot = useStore((s) => s.snapshot);
  const lastError = useStore((s) => s.lastError);
  const clock = useClock();

  const sessions = Object.values(snapshot?.runtimeState?.sessions ?? {});
  const runningCount = sessions.filter((s) => isLiveStatus(s.status)).length;

  let tone: string;
  let label: string;
  let Icon = Wifi;
  switch (status.kind) {
    case "open":
      tone = "text-emerald-400";
      label = "Connected";
      Icon = Wifi;
      break;
    case "connecting":
    case "idle":
      tone = "text-amber-400";
      label = "Connecting…";
      Icon = Wifi;
      break;
    case "closed":
      tone = "text-amber-400";
      label = "Reconnecting…";
      Icon = WifiOff;
      break;
    case "unauthorized":
      tone = "text-red-400";
      label = "Not paired";
      Icon = WifiOff;
      break;
  }

  return (
    <footer className="flex items-center justify-between px-3 py-1 bg-zinc-950 border-t border-zinc-800 text-[11px] text-zinc-500 shrink-0">
      <div className="flex items-center gap-4 min-w-0">
        <span className={`flex items-center gap-1.5 ${tone}`}>
          <Icon className="size-3" />
          <span>{label}</span>
        </span>
        <span className="flex items-center gap-1.5">
          <Server className="size-3" />
          <span>
            {runningCount} running · {sessions.length} total
          </span>
        </span>
        {lastError && (
          <span className="flex items-center gap-1.5 text-red-400 truncate">
            <Activity className="size-3 shrink-0" />
            <span className="truncate">{lastError}</span>
          </span>
        )}
      </div>
      <span className="tabular-nums">{clock}</span>
    </footer>
  );
}
