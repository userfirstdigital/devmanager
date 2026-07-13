import { useEffect, useState } from "react";

import type { InterfaceDensity } from "../sessions/timeline/eventRenderers";

const STORAGE_KEY = "devmanager-interface-density:v1";
const CHANGE_EVENT = "devmanager-interface-density-change";

function isDensity(value: unknown): value is InterfaceDensity {
  return value === "minimal" || value === "calm" || value === "full";
}

export function loadDensity(): InterfaceDensity {
  try {
    const value = globalThis.localStorage?.getItem(STORAGE_KEY);
    return isDensity(value) ? value : "calm";
  } catch {
    return "calm";
  }
}

export function saveDensity(density: InterfaceDensity): void {
  try {
    globalThis.localStorage?.setItem(STORAGE_KEY, density);
  } catch {
    // Presentation preferences are best effort.
  }
  globalThis.dispatchEvent?.(new CustomEvent(CHANGE_EVENT, { detail: density }));
}

export function useDensityPreference(): [InterfaceDensity, (density: InterfaceDensity) => void] {
  const [density, setDensity] = useState<InterfaceDensity>(() => loadDensity());
  useEffect(() => {
    const onChange = (event: Event) => {
      const next = (event as CustomEvent<unknown>).detail;
      if (isDensity(next)) setDensity(next);
    };
    globalThis.addEventListener?.(CHANGE_EVENT, onChange);
    return () => globalThis.removeEventListener?.(CHANGE_EVENT, onChange);
  }, []);
  const update = (next: InterfaceDensity) => {
    setDensity(next);
    saveDensity(next);
  };
  return [density, update];
}
