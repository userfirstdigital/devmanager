/**
 * Bounded streamed response body reader. Fail-closed when no ReadableStream
 * reader is available — never fall back to unbounded response.text().
 */

export class BoundedResponseError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "BoundedResponseError";
  }
}

/** Bound waiting independently of whether the underlying browser API honors abort. */
export async function withResponseAbort<T>(
  work: Promise<T>, signal: AbortSignal, message = "response deadline exceeded",
): Promise<T> {
  let rejectAbort!: (error: Error) => void;
  const aborted = new Promise<never>((_, reject) => { rejectAbort = reject; });
  const onAbort = () => rejectAbort(new BoundedResponseError(message));
  signal.addEventListener("abort", onAbort, { once: true });
  if (signal.aborted) onAbort();
  try {
    const result = await Promise.race([work, aborted]);
    if (signal.aborted) throw new BoundedResponseError(message);
    return result;
  } finally {
    signal.removeEventListener("abort", onAbort);
  }
}

/**
 * Read a response body under one absolute AbortSignal with a hard byte cap.
 * Cancels the reader when the deadline fires or the cap is exceeded.
 */
export async function readBoundedResponseText(
  response: Response,
  maxBytes: number,
  signal: AbortSignal,
  rejectedMessage = "response body rejected",
): Promise<string> {
  if (signal.aborted) {
    throw new BoundedResponseError(rejectedMessage);
  }
  const reader = response.body?.getReader();
  if (!reader) {
    // FAIL CLOSED: a Response without a stream reader cannot produce a
    // validated bounded body for Connect/fleet/HTML contracts.
    throw new BoundedResponseError(rejectedMessage);
  }
  const chunks: Uint8Array[] = [];
  let total = 0;
  const onAbort = () => {
    void reader.cancel().catch(() => undefined);
  };
  signal.addEventListener("abort", onAbort, { once: true });
  try {
    if (signal.aborted) {
      onAbort();
      throw new BoundedResponseError(rejectedMessage);
    }
    for (;;) {
      const { done, value } = await withResponseAbort(reader.read(), signal, rejectedMessage);
      if (done) break;
      if (!value) continue;
      total += value.byteLength;
      if (total > maxBytes) {
        void reader.cancel().catch(() => undefined);
        throw new BoundedResponseError(rejectedMessage);
      }
      chunks.push(value);
    }
  } catch (error) {
    void reader.cancel().catch(() => undefined);
    if (error instanceof BoundedResponseError) throw error;
    throw new BoundedResponseError(rejectedMessage);
  } finally {
    signal.removeEventListener("abort", onAbort);
    try { reader.releaseLock(); } catch { /* A rejected implementation may retain a pending read. */ }
  }
  if (signal.aborted) throw new BoundedResponseError(rejectedMessage);
  const merged = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    merged.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return new TextDecoder().decode(merged);
}
