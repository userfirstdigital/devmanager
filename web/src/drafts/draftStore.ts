const STORAGE_KEY = "devmanager-native-drafts:v1";
const VERSION = 1;
const MAX_DRAFT_BYTES = 32 * 1024;
const DRAFT_TTL_MS = 7 * 24 * 60 * 60 * 1000;

interface StoredDraft {
  text: string;
  updatedAt: number;
}

interface StoredDrafts {
  version: typeof VERSION;
  runtimeInstanceId: string;
  drafts: Record<string, StoredDraft>;
}

function storage(): Storage | null {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
}

function readStoredDrafts(): StoredDrafts | null {
  try {
    const raw = storage()?.getItem(STORAGE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<StoredDrafts>;
    if (
      parsed.version !== VERSION ||
      typeof parsed.runtimeInstanceId !== "string" ||
      !parsed.drafts ||
      typeof parsed.drafts !== "object"
    ) {
      return null;
    }
    return parsed as StoredDrafts;
  } catch {
    return null;
  }
}

function writeStoredDrafts(value: StoredDrafts | null): void {
  try {
    const target = storage();
    if (!target) return;
    if (!value || Object.keys(value.drafts).length === 0) {
      target.removeItem(STORAGE_KEY);
    } else {
      target.setItem(STORAGE_KEY, JSON.stringify(value));
    }
  } catch {
    // Draft persistence is best effort in private/quota-limited contexts.
  }
}

function truncateUtf8(text: string, maxBytes = MAX_DRAFT_BYTES): string {
  if (new TextEncoder().encode(text).byteLength <= maxBytes) return text;
  let kept = "";
  let bytes = 0;
  for (const character of text) {
    const characterBytes = new TextEncoder().encode(character).byteLength;
    if (bytes + characterBytes > maxBytes) break;
    kept += character;
    bytes += characterBytes;
  }
  return kept;
}

function removeExpired(value: StoredDrafts, now: number): StoredDrafts {
  const drafts = Object.fromEntries(
    Object.entries(value.drafts).filter(
      ([, draft]) =>
        typeof draft?.text === "string" &&
        typeof draft.updatedAt === "number" &&
        now - draft.updatedAt <= DRAFT_TTL_MS,
    ),
  );
  return { ...value, drafts };
}

export function saveDraft(
  runtimeInstanceId: string,
  stableSessionKey: string,
  text: string,
  now = Date.now(),
): void {
  if (!runtimeInstanceId || !stableSessionKey) return;
  const previous = readStoredDrafts();
  const current: StoredDrafts =
    previous?.runtimeInstanceId === runtimeInstanceId
      ? removeExpired(previous, now)
      : { version: VERSION, runtimeInstanceId, drafts: {} };
  const drafts = { ...current.drafts };
  const bounded = truncateUtf8(text);
  if (bounded) drafts[stableSessionKey] = { text: bounded, updatedAt: now };
  else delete drafts[stableSessionKey];
  writeStoredDrafts({ ...current, drafts });
}

export function loadDraft(
  runtimeInstanceId: string,
  stableSessionKey: string,
  now = Date.now(),
): string | null {
  const value = readStoredDrafts();
  if (!value || value.runtimeInstanceId !== runtimeInstanceId) return null;
  const cleaned = removeExpired(value, now);
  if (Object.keys(cleaned.drafts).length !== Object.keys(value.drafts).length) {
    writeStoredDrafts(cleaned);
  }
  return cleaned.drafts[stableSessionKey]?.text ?? null;
}

export function removeDraft(runtimeInstanceId: string, stableSessionKey: string): void {
  const value = readStoredDrafts();
  if (!value || value.runtimeInstanceId !== runtimeInstanceId) return;
  const drafts = { ...value.drafts };
  delete drafts[stableSessionKey];
  writeStoredDrafts({ ...value, drafts });
}

export function clearOtherRuntimes(runtimeInstanceId: string): void {
  const value = readStoredDrafts();
  if (value && value.runtimeInstanceId !== runtimeInstanceId) writeStoredDrafts(null);
}

export function pruneDrafts(
  runtimeInstanceId: string,
  liveSessionKeys: ReadonlySet<string>,
  now = Date.now(),
): void {
  const value = readStoredDrafts();
  if (!value || value.runtimeInstanceId !== runtimeInstanceId) return;
  const cleaned = removeExpired(value, now);
  const drafts = Object.fromEntries(
    Object.entries(cleaned.drafts).filter(([key]) => liveSessionKeys.has(key)),
  );
  writeStoredDrafts({ ...cleaned, drafts });
}
