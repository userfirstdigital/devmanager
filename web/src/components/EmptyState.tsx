import { Terminal } from "lucide-react";
import { useStore } from "../store";

export function EmptyState() {
  const snapshot = useStore((s) => s.snapshot);
  const portStatuses = snapshot?.portStatuses ?? {};
  const runningPorts = Object.values(portStatuses).filter((p) => p.inUse);
  const host = typeof location !== "undefined" ? location.hostname : "localhost";

  return (
    <div className="flex-1 flex flex-col items-center justify-center px-6 py-12 gap-6">
      <div className="flex flex-col items-center gap-3 text-center">
        <div className="size-12 rounded-full bg-zinc-800 flex items-center justify-center">
          <Terminal className="size-6 text-zinc-500" />
        </div>
        <h2 className="text-sm font-medium text-zinc-300">
          Pick a command from the sidebar
        </h2>
        <p className="text-xs text-zinc-500 max-w-xs">
          Tap any command to open its terminal. Start it with the play button
          if it isn't already running.
        </p>
      </div>

      {runningPorts.length > 0 && (
        <section className="w-full max-w-md">
          <h3 className="text-[11px] font-semibold uppercase tracking-wide text-zinc-500 mb-2">
            Running dev servers
          </h3>
          <div className="flex flex-col gap-1.5">
            {runningPorts
              .sort((a, b) => a.port - b.port)
              .map((status) => (
                <a
                  key={status.port}
                  href={`http://${host}:${status.port}`}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="flex items-center gap-3 px-3 py-2 rounded bg-zinc-800 border border-zinc-700 hover:border-indigo-500 transition-colors"
                >
                  <span className="text-xs font-mono text-indigo-400">
                    :{status.port}
                  </span>
                  <span className="text-xs text-zinc-300 flex-1 truncate">
                    {status.processName ?? "running"}
                  </span>
                  {status.pid != null && (
                    <span className="text-[10px] text-zinc-500">
                      pid {status.pid}
                    </span>
                  )}
                </a>
              ))}
          </div>
        </section>
      )}
    </div>
  );
}
