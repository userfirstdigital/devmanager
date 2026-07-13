export const UPDATE_ACTIVATION_REQUEST =
  "DEVMANAGER_UPDATE_ACTIVATION_REQUEST" as const;
export const UPDATE_SAFETY_QUERY = "DEVMANAGER_UPDATE_SAFETY_QUERY" as const;
export const UPDATE_SAFETY_ACK = "DEVMANAGER_UPDATE_SAFETY_ACK" as const;
export const UPDATE_ACTIVATION_RESULT =
  "DEVMANAGER_UPDATE_ACTIVATION_RESULT" as const;

export interface UpdateActivationRequest {
  type: typeof UPDATE_ACTIVATION_REQUEST;
  nonce: string;
}

export interface UpdateSafetyQuery {
  type: typeof UPDATE_SAFETY_QUERY;
  nonce: string;
}

export interface UpdateSafetyAck {
  type: typeof UPDATE_SAFETY_ACK;
  nonce: string;
  safe: boolean;
}

export interface UpdateActivationResult {
  type: typeof UPDATE_ACTIVATION_RESULT;
  nonce: string;
  activated: boolean;
}

export interface WorkerUpdateClient {
  id: string;
  visibilityState?: "visible" | "hidden";
  postMessage(message: UpdateSafetyQuery): void;
}

interface ActiveAttempt {
  nonce: string;
  responses: Map<string, boolean>;
  expected: Set<string>;
  finishWait: (() => void) | null;
}

export function isUpdateActivationRequest(
  value: unknown,
): value is UpdateActivationRequest {
  return (
    typeof value === "object" &&
    value !== null &&
    (value as { type?: unknown }).type === UPDATE_ACTIVATION_REQUEST &&
    typeof (value as { nonce?: unknown }).nonce === "string" &&
    (value as { nonce: string }).nonce.length > 0
  );
}

export function isUpdateSafetyAck(value: unknown): value is UpdateSafetyAck {
  return (
    typeof value === "object" &&
    value !== null &&
    (value as { type?: unknown }).type === UPDATE_SAFETY_ACK &&
    typeof (value as { nonce?: unknown }).nonce === "string" &&
    typeof (value as { safe?: unknown }).safe === "boolean"
  );
}

export function isUpdateSafetyQuery(value: unknown): value is UpdateSafetyQuery {
  return (
    typeof value === "object" &&
    value !== null &&
    (value as { type?: unknown }).type === UPDATE_SAFETY_QUERY &&
    typeof (value as { nonce?: unknown }).nonce === "string" &&
    (value as { nonce: string }).nonce.length > 0
  );
}

export function isUpdateActivationResult(
  value: unknown,
): value is UpdateActivationResult {
  return (
    typeof value === "object" &&
    value !== null &&
    (value as { type?: unknown }).type === UPDATE_ACTIVATION_RESULT &&
    typeof (value as { nonce?: unknown }).nonce === "string" &&
    typeof (value as { activated?: unknown }).activated === "boolean"
  );
}

export function requestWaitingWorkerActivation({
  worker,
  messages,
  nonce,
  timeoutMs = 3_000,
}: {
  worker: { postMessage(message: UpdateActivationRequest): void };
  messages: {
    addEventListener(type: "message", listener: (event: MessageEvent) => void): void;
    removeEventListener(type: "message", listener: (event: MessageEvent) => void): void;
  };
  nonce: string;
  timeoutMs?: number;
}): Promise<boolean> {
  return new Promise<boolean>((resolve) => {
    let timer: ReturnType<typeof globalThis.setTimeout> | null = null;
    let finished = false;
    const finish = (activated: boolean) => {
      if (finished) return;
      finished = true;
      if (timer !== null) globalThis.clearTimeout(timer);
      messages.removeEventListener("message", onMessage);
      resolve(activated);
    };
    const onMessage = (event: MessageEvent) => {
      if (
        isUpdateActivationResult(event.data) &&
        event.data.nonce === nonce
      ) {
        finish(event.data.activated);
      }
    };

    messages.addEventListener("message", onMessage);
    timer = globalThis.setTimeout(() => finish(false), timeoutMs);
    try {
      worker.postMessage({ type: UPDATE_ACTIVATION_REQUEST, nonce });
    } catch {
      finish(false);
    }
  });
}

export function createWorkerUpdateGate({
  listClients,
  skipWaiting,
  ackTimeoutMs = 1_500,
  maxEnumerationRounds = 8,
}: {
  listClients: () => Promise<WorkerUpdateClient[]>;
  skipWaiting: () => Promise<void>;
  ackTimeoutMs?: number;
  maxEnumerationRounds?: number;
}) {
  let activeAttempt: ActiveAttempt | null = null;

  const waitForResponses = async (
    attempt: ActiveAttempt,
    expected: Set<string>,
  ): Promise<void> => {
    attempt.expected = expected;
    if (
      [...expected].every((id) => attempt.responses.has(id)) ||
      [...expected].some((id) => attempt.responses.get(id) === false)
    ) {
      return;
    }

    await new Promise<void>((resolve) => {
      let finished = false;
      const timer = globalThis.setTimeout(() => finish(), ackTimeoutMs);
      const finish = () => {
        if (finished) return;
        finished = true;
        globalThis.clearTimeout(timer);
        attempt.finishWait = null;
        resolve();
      };
      attempt.finishWait = finish;
    });
  };

  const requestActivation = async (nonce: string): Promise<boolean> => {
    if (!nonce || activeAttempt) return false;
    const attempt: ActiveAttempt = {
      nonce,
      responses: new Map(),
      expected: new Set(),
      finishWait: null,
    };
    activeAttempt = attempt;
    const queried = new Set<string>();
    const hasUnsafeVisibleClient = (clients: WorkerUpdateClient[]) =>
      clients.some(
        (client) =>
          attempt.responses.get(client.id) === false ||
          (!attempt.responses.has(client.id) &&
            client.visibilityState !== "hidden"),
      );

    try {
      for (let round = 0; round < maxEnumerationRounds; round += 1) {
        const clients = await listClients();
        const expected = new Set(clients.map((client) => client.id));
        for (const client of clients) {
          if (queried.has(client.id)) continue;
          queried.add(client.id);
          client.postMessage({ type: UPDATE_SAFETY_QUERY, nonce });
        }

        await waitForResponses(attempt, expected);

        // A tab may have closed, opened, or changed visibility while ACKs were
        // in flight. Only this fresh set is allowed to decide activation.
        const liveClients = await listClients();
        const liveIds = new Set(liveClients.map((client) => client.id));
        if (liveClients.some((client) => !queried.has(client.id))) continue;
        if (hasUnsafeVisibleClient(liveClients)) return false;

        // Re-enumerate immediately before skipWaiting. A changed set starts a
        // new query round; closed clients are intentionally absent.
        const finalClients = await listClients();
        const finalIds = new Set(finalClients.map((client) => client.id));
        if (
          finalIds.size !== liveIds.size ||
          [...finalIds].some((id) => !liveIds.has(id))
        ) {
          continue;
        }
        if (hasUnsafeVisibleClient(finalClients)) return false;

        await skipWaiting();
        return true;
      }
      return false;
    } finally {
      activeAttempt?.finishWait?.();
      activeAttempt = null;
    }
  };

  return {
    requestActivation,
    acknowledge(nonce: string, clientId: string, safe: boolean): void {
      const attempt = activeAttempt;
      if (
        !attempt ||
        attempt.nonce !== nonce ||
        !attempt.expected.has(clientId)
      ) {
        return;
      }
      attempt.responses.set(clientId, safe);
      if (
        !safe ||
        [...attempt.expected].every((id) => attempt.responses.has(id))
      ) {
        attempt.finishWait?.();
      }
    },
  };
}

export function createLocalReloadGate({
  isVisible,
  readSafetyState,
  reload,
}: {
  isVisible: () => boolean;
  readSafetyState: () => {
    hasDraft: boolean;
    pendingMutations: number;
    selectedAttachments?: number;
    attachmentLoads?: number;
  };
  reload: () => void;
}) {
  let reloadPending = false;

  const attemptReload = (): boolean => {
    const safety = readSafetyState();
    if (
      !reloadPending ||
      !isVisible() ||
      safety.hasDraft ||
      safety.pendingMutations !== 0 ||
      (safety.selectedAttachments ?? 0) !== 0 ||
      (safety.attachmentLoads ?? 0) !== 0
    ) {
      return false;
    }
    reloadPending = false;
    reload();
    return true;
  };

  return {
    notifyControllerChanged(): boolean {
      reloadPending = true;
      return attemptReload();
    },
    notifySafePoint: attemptReload,
    hasPendingReload: () => reloadPending,
  };
}
