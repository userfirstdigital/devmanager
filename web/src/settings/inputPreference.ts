import { useCallback, useState } from "react";

export type ReturnBehavior = "newline" | "send";
export type TerminalPreference = "automatic" | "raw";

const RETURN_BEHAVIOR_KEY = "devmanager.returnBehavior";
const TERMINAL_PREFERENCE_KEY = "devmanager.terminalPreference";

export function normalizeReturnBehavior(value: unknown): ReturnBehavior {
  return value === "send" ? "send" : "newline";
}

export function normalizeTerminalPreference(value: unknown): TerminalPreference {
  return value === "raw" ? "raw" : "automatic";
}

function readPreference<T>(key: string, normalize: (value: unknown) => T): T {
  try {
    return normalize(globalThis.localStorage?.getItem(key));
  } catch {
    return normalize(null);
  }
}

function persistPreference(key: string, value: string): void {
  try {
    globalThis.localStorage?.setItem(key, value);
  } catch {
    // Presentation preferences are best effort in private/quota-limited modes.
  }
}

export function useReturnBehavior(): [
  ReturnBehavior,
  (value: ReturnBehavior) => void,
] {
  const [value, setValue] = useState<ReturnBehavior>(() =>
    readPreference(RETURN_BEHAVIOR_KEY, normalizeReturnBehavior),
  );
  const update = useCallback((next: ReturnBehavior) => {
    setValue(next);
    persistPreference(RETURN_BEHAVIOR_KEY, next);
  }, []);
  return [value, update];
}

export function useTerminalPreference(): [
  TerminalPreference,
  (value: TerminalPreference) => void,
] {
  const [value, setValue] = useState<TerminalPreference>(() =>
    readPreference(TERMINAL_PREFERENCE_KEY, normalizeTerminalPreference),
  );
  const update = useCallback((next: TerminalPreference) => {
    setValue(next);
    persistPreference(TERMINAL_PREFERENCE_KEY, next);
  }, []);
  return [value, update];
}
