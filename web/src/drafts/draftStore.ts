const STORAGE_KEY = "devmanager-native-drafts:v1";
const HANDOFF_STORAGE_KEY = "devmanager-compatible-draft-handoff:v1";
const VERSION = 1;
const HANDOFF_VERSION = 1;
const MAX_DRAFT_BYTES = 32 * 1024;
const MAX_HANDOFF_BYTES = 512 * 1024;
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

interface StoredDraftHandoff {
  version: typeof HANDOFF_VERSION;
  runtimeInstanceId: string;
  drafts: Record<string, string>;
}

function storage(): Storage | null {
  try {
    return globalThis.localStorage ?? null;
  } catch {
    return null;
  }
}

function handoffStorage(): Storage | null {
  try {
    return globalThis.sessionStorage ?? null;
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

function readDraftHandoff(): StoredDraftHandoff | null {
  try {
    const raw = handoffStorage()?.getItem(HANDOFF_STORAGE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<StoredDraftHandoff>;
    if (
      parsed.version !== HANDOFF_VERSION ||
      typeof parsed.runtimeInstanceId !== "string" ||
      !parsed.drafts ||
      typeof parsed.drafts !== "object" ||
      !Object.values(parsed.drafts).every((text) => typeof text === "string")
    ) {
      return null;
    }
    return parsed as StoredDraftHandoff;
  } catch {
    return null;
  }
}

function writeDraftHandoff(value: StoredDraftHandoff | null): boolean {
  try {
    const target = handoffStorage();
    if (!target) return false;
    if (!value || Object.keys(value.drafts).length === 0) {
      target.removeItem(HANDOFF_STORAGE_KEY);
      return true;
    }
    const serialized = JSON.stringify(value);
    if (new TextEncoder().encode(serialized).byteLength > MAX_HANDOFF_BYTES) {
      return false;
    }
    target.setItem(HANDOFF_STORAGE_KEY, serialized);
    return target.getItem(HANDOFF_STORAGE_KEY) === serialized;
  } catch {
    return false;
  }
}

function exactDraftRecordsMatch(
  left: Record<string, string>,
  right: Record<string, string>,
): boolean {
  const leftKeys = Object.keys(left).sort();
  const rightKeys = Object.keys(right).sort();
  return (
    leftKeys.length === rightKeys.length &&
    leftKeys.every(
      (key, index) =>
        key === rightKeys[index] && left[key] === right[rightKeys[index]],
    )
  );
}

function nonEmptyDrafts(
  drafts: Record<string, string>,
): Record<string, string> {
  return Object.fromEntries(
    Object.entries(drafts).filter(([, text]) => text !== ""),
  );
}

export function hasExactDraftHandoff(
  runtimeInstanceId: string,
  drafts: Record<string, string>,
): boolean {
  const exactDrafts = nonEmptyDrafts(drafts);
  if (!runtimeInstanceId || Object.keys(exactDrafts).length === 0) return false;
  const handoff = readDraftHandoff();
  return Boolean(
    handoff &&
      handoff.runtimeInstanceId === runtimeInstanceId &&
      exactDraftRecordsMatch(handoff.drafts, exactDrafts),
  );
}

export function stageDraftHandoff(
  runtimeInstanceId: string,
  drafts: Record<string, string>,
): boolean {
  const exactDrafts = nonEmptyDrafts(drafts);
  if (Object.keys(exactDrafts).length === 0) {
    return writeDraftHandoff(null);
  }
  if (!runtimeInstanceId) return false;
  if (
    Object.values(exactDrafts).some(
      (text) => new TextEncoder().encode(text).byteLength > MAX_DRAFT_BYTES,
    )
  ) {
    return false;
  }
  if (
    !writeDraftHandoff({
      version: HANDOFF_VERSION,
      runtimeInstanceId,
      drafts: exactDrafts,
    })
  ) {
    return false;
  }
  return hasExactDraftHandoff(runtimeInstanceId, exactDrafts);
}

function takeDraftHandoff(
  runtimeInstanceId: string,
  stableSessionKey: string,
): string | null {
  const handoff = readDraftHandoff();
  if (!handoff || handoff.runtimeInstanceId !== runtimeInstanceId) return null;
  const text = handoff.drafts[stableSessionKey];
  if (typeof text !== "string") return null;
  const drafts = { ...handoff.drafts };
  delete drafts[stableSessionKey];
  writeDraftHandoff(
    Object.keys(drafts).length === 0 ? null : { ...handoff, drafts },
  );
  return text;
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
  const handedOff = takeDraftHandoff(runtimeInstanceId, stableSessionKey);
  if (handedOff !== null) return handedOff;
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
  if (value?.runtimeInstanceId === runtimeInstanceId) {
    const drafts = { ...value.drafts };
    delete drafts[stableSessionKey];
    writeStoredDrafts({ ...value, drafts });
  }
  const handoff = readDraftHandoff();
  if (handoff?.runtimeInstanceId === runtimeInstanceId) {
    const handedOffDrafts = { ...handoff.drafts };
    delete handedOffDrafts[stableSessionKey];
    writeDraftHandoff(
      Object.keys(handedOffDrafts).length === 0
        ? null
        : { ...handoff, drafts: handedOffDrafts },
    );
  }
}

export function clearOtherRuntimes(runtimeInstanceId: string): void {
  const value = readStoredDrafts();
  if (value && value.runtimeInstanceId !== runtimeInstanceId) writeStoredDrafts(null);
  const handoff = readDraftHandoff();
  if (handoff && handoff.runtimeInstanceId !== runtimeInstanceId) {
    writeDraftHandoff(null);
  }
}

export function pruneDrafts(
  runtimeInstanceId: string,
  liveSessionKeys: ReadonlySet<string>,
  now = Date.now(),
): void {
  const value = readStoredDrafts();
  if (value?.runtimeInstanceId === runtimeInstanceId) {
    const cleaned = removeExpired(value, now);
    const drafts = Object.fromEntries(
      Object.entries(cleaned.drafts).filter(([key]) =>
        liveSessionKeys.has(key),
      ),
    );
    writeStoredDrafts({ ...cleaned, drafts });
  }
  const handoff = readDraftHandoff();
  if (handoff?.runtimeInstanceId === runtimeInstanceId) {
    const handedOffDrafts = Object.fromEntries(
      Object.entries(handoff.drafts).filter(([key]) =>
        liveSessionKeys.has(key),
      ),
    );
    writeDraftHandoff(
      Object.keys(handedOffDrafts).length === 0
        ? null
        : { ...handoff, drafts: handedOffDrafts },
    );
  }
}
