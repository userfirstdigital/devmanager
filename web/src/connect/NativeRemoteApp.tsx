import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import {
  Archive,
  ArrowLeft,
  Check,
  ChevronRight,
  CircleAlert,
  MessageSquare,
  MoreVertical,
  Pencil,
  RotateCcw,
  Search,
  Send,
  Server,
  SquareTerminal,
  Trash2,
  WifiOff,
} from "lucide-react";

import { MarkdownMessage } from "../tasks/timeline/MarkdownMessage";
import { protocolUuid } from "./hostOutput";
import type { NativeFleetEntrySnapshot } from "./nativeHostRegistry";
import type {
  NativeHostSession,
  NativeHostSessionView,
  NativeSendFailure,
  NativeTaskMutation,
} from "./nativeSession";
import { buildNativeTimeline } from "./nativeTimeline";
import type { NativeTerminalKey, NativeUuid, SemanticJournalFact } from "./nativeProtocol";
import {
  scopedHostTaskKey,
  scopeHostTask,
  type ScopedHostTaskRef,
} from "./scopedHostTask";
import "./NativeRemoteApp.css";

export interface NativeRemoteAppProps {
  hostPublicId: string;
  hostLabel?: string;
  /** Retained by the bootstrap; this view owns no transport lifecycle. */
  session: NativeHostSession;
  /**
   * Optional fleet snapshots. When omitted, the single page-host session is
   * used alone (legacy tests and single-host pages).
   */
  fleetEntries?: readonly NativeFleetEntrySnapshot[];
  /** Owner sessions keyed by hostPublicId for multi-host actions. */
  hostSessions?: ReadonlyMap<string, NativeHostSession>;
  /** Page-host id for legacy `/tasks/:taskId` migration. Defaults to hostPublicId. */
  pageHostPublicId?: string;
  onRetryHost?: (hostPublicId: string) => void;
  onSubmitPairGrant?: (hostPublicId: string, grant: string) => void;
  onShowPagePairing?: () => void;
}

type TaskMeta = NativeHostSessionView["tasks"] extends ReadonlyMap<
  NativeUuid,
  infer Value
>
  ? Value
  : never;

type MergedTaskRow = TaskMeta & {
  hostPublicId: NativeUuid;
  hostLabel: string;
};

const LAST_SELECTED_TASK_KEY_PREFIX = "devmanager.connect.native.last-selected.v1:";

export function NativeRemoteApp({
  hostPublicId,
  hostLabel,
  session,
  fleetEntries,
  hostSessions,
  pageHostPublicId,
  onRetryHost,
  onSubmitPairGrant,
  onShowPagePairing,
}: NativeRemoteAppProps) {
  const pageHost = protocolUuid(pageHostPublicId ?? hostPublicId) ?? hostPublicId;
  const [pageView, setPageView] = useState(() => session.view());
  const [fleetTick, setFleetTick] = useState(0);
  const hosts = useMemo(() => {
    if (fleetEntries && fleetEntries.length > 0) {
      return fleetEntries;
    }
    return [
      {
        descriptor: {
          hostPublicId,
          hostPublicKey: "",
          origin: "",
          label: hostLabel ?? "This device",
          generation: 1,
          protocolMajor: 1 as const,
          protocolMinor: 0,
          isPageHost: true,
        },
        view: pageView,
        hydrationKnown: true,
        pairingState: "ready" as const,
        transportAttached: pageView.connectionStatus === "ready",
        authenticated: pageView.connectionStatus === "ready",
        notice: pageView.lastError,
        cacheAvailable: true,
      },
    ];
  }, [fleetEntries, hostPublicId, hostLabel, pageView, fleetTick]);

  const resolveSession = (ownerHostId: string): NativeHostSession | null => {
    if (hostSessions?.has(ownerHostId)) {
      return hostSessions.get(ownerHostId) ?? null;
    }
    if (ownerHostId === hostPublicId || ownerHostId === pageHost) return session;
    return null;
  };

  const initialRoute = readNativeTaskRoute(pageHost);
  const [selected, setSelected] = useState<ScopedHostTaskRef | null>(() => {
    if (initialRoute.kind === "selected") return initialRoute.ref;
    if (initialRoute.kind === "unavailable") return null;
    const remembered = readRememberedTask(pageHost);
    return remembered
      ? { hostPublicId: pageHost, taskId: remembered }
      : null;
  });
  const [routeUnavailable, setRouteUnavailable] = useState(
    () => initialRoute.kind === "unavailable",
  );
  const [searchOpen, setSearchOpen] = useState(false);
  const [search, setSearch] = useState("");
  const [hostsOpen, setHostsOpen] = useState(false);
  const [archiveOpen, setArchiveOpen] = useState(false);
  const [composer, setComposer] = useState<{
    ownerKey: string | null;
    text: string;
  }>({ ownerKey: null, text: "" });
  const [sendingByKey, setSendingByKey] = useState<ReadonlyMap<string, number>>(
    () => new Map(),
  );
  const [mutatingByKey, setMutatingByKey] = useState<ReadonlyMap<string, number>>(
    () => new Map(),
  );
  const [localNotice, setLocalNotice] = useState<string | null>(null);
  const selectedRef = useRef(selected);
  const draftTextByKeyRef = useRef(new Map<string, string>());
  const draftVersionByKeyRef = useRef(new Map<string, number>());
  const sendingIdentityByKeyRef = useRef(new Map<string, number>());
  const mutatingIdentityByKeyRef = useRef(new Map<string, number>());
  const nextSendIdentityRef = useRef(0);
  const nextMutateIdentityRef = useRef(0);

  useEffect(() => session.subscribe(setPageView), [session]);
  useEffect(() => {
    // Fleet parent re-renders push new fleetEntries; bump for derived hosts.
    if (fleetEntries) setFleetTick((value) => value + 1);
  }, [fleetEntries]);

  useEffect(() => {
    selectedRef.current = selected;
  }, [selected]);

  useEffect(() => {
    const onPopState = () => {
      const parsed = readNativeTaskRoute(pageHost);
      if (parsed.kind === "unavailable") {
        selectedRef.current = null;
        setSelected(null);
        setRouteUnavailable(true);
        setLocalNotice(null);
        return;
      }
      setRouteUnavailable(false);
      selectedRef.current = parsed.ref;
      setLocalNotice(null);
      setSelected(parsed.ref);
    };
    window.addEventListener("popstate", onPopState);
    return () => window.removeEventListener("popstate", onPopState);
  }, [pageHost]);

  useEffect(() => {
    if (!selected) return;
    rememberSelectedTask(selected.hostPublicId, selected.taskId);
    replaceNativeTaskRoute(selected);
  }, [selected]);

  const hostLabelById = useMemo(() => {
    const map = new Map<string, string>();
    for (const entry of hosts) {
      map.set(entry.descriptor.hostPublicId, entry.descriptor.label);
    }
    return map;
  }, [hosts]);

  const ownerEntry = selected
    ? hosts.find((entry) => entry.descriptor.hostPublicId === selected.hostPublicId) ??
      null
    : null;
  const ownerView = ownerEntry?.view ?? null;
  const selectedOwnerMissing = Boolean(selected && !ownerEntry && hosts.length > 0);

  const allTasks = useMemo(() => {
    const rows: MergedTaskRow[] = [];
    for (const entry of hosts) {
      for (const task of entry.view.tasks.values()) {
        if (task.lifecycle === "deleted") continue;
        rows.push({
          ...task,
          hostPublicId: entry.descriptor.hostPublicId,
          hostLabel: entry.descriptor.label,
        });
      }
    }
    return rows.sort(compareMergedTasks);
  }, [hosts]);

  const filteredTasks = useMemo(() => {
    const needle = search.trim().toLocaleLowerCase();
    return needle
      ? allTasks.filter((task) => task.title?.toLocaleLowerCase().includes(needle))
      : allTasks;
  }, [allTasks, search]);
  const inboxTasks = filteredTasks.filter(
    (task) => !isDoneTask(task) && !isArchivedTask(task),
  );
  const doneTasks = filteredTasks.filter(isDoneTask);
  const archivedTasks = filteredTasks.filter(isArchivedTask);
  const showHostBadge = hosts.length > 1;

  const selectedTask = selected
    ? ownerView?.tasks.get(selected.taskId) ?? null
    : null;
  const selectedConversation = selected
    ? ownerView?.conversations.get(selected.taskId) ?? null
    : null;
  const selectedOutbox = useMemo(
    () =>
      selected && ownerView
        ? [...ownerView.outbox.values()].filter(
            (item) => item.taskId === selected.taskId,
          )
        : [],
    [selected, ownerView],
  );
  const selectedKey = selected ? scopedHostTaskKey(selected) : null;
  // Synchronously retarget composer to the exact owner key before paint.
  if (selected !== null && selectedKey !== null && composer.ownerKey !== selectedKey) {
    const seeded =
      draftTextByKeyRef.current.get(selectedKey) ??
      ownerView?.drafts.get(selected.taskId)?.text ??
      "";
    setComposer({ ownerKey: selectedKey, text: seeded });
  }
  const visibleComposer =
    selectedKey !== null && composer.ownerKey === selectedKey
      ? composer.text
      : "";
  const isSelectedSending =
    selectedKey !== null && sendingByKey.has(selectedKey);
  const isSelectedMutating =
    selectedKey !== null && mutatingByKey.has(selectedKey);
  // Only known live lifecycle states may send. In particular, an absent or
  // deleted task must not regain a composer just because its owner is online.
  // Done/settled stays sendable — host SendNow restores atomically.
  const lifecycleCanSend =
    selectedTask?.lifecycle === "open" ||
    selectedTask?.lifecycle === "settled";
  const canSend =
    !isSelectedSending &&
    !isSelectedMutating &&
    lifecycleCanSend &&
    selectedOutbox.length === 0 &&
    ownerView?.connectionStatus === "ready" &&
    ownerView?.syncStatus === "live" &&
    visibleComposer.trim().length > 0;

  useEffect(() => {
    if (!selected || !ownerView || !selectedKey) return;
    // Seed only when this owner has no newer local typing.
    if (draftVersionByKeyRef.current.has(selectedKey)) return;
    const nextDraft = ownerView.drafts.get(selected.taskId)?.text ?? "";
    draftTextByKeyRef.current.set(selectedKey, nextDraft);
    setComposer({ ownerKey: selectedKey, text: nextDraft });
  }, [selectedKey, ownerView?.drafts, selected, selectedKey]);

  const ownerSessionForWatch = (() => {
    if (!selected) return null;
    if (hostSessions?.has(selected.hostPublicId)) {
      return hostSessions.get(selected.hostPublicId) ?? null;
    }
    if (
      selected.hostPublicId === hostPublicId ||
      selected.hostPublicId === pageHost
    ) {
      return session;
    }
    return null;
  })();

  useEffect(() => {
    if (!selected || !ownerSessionForWatch) return;
    const taskId = selected.taskId;
    const owner = ownerSessionForWatch;
    let current = true;
    void owner.watchTask(taskId).catch(() => {
      if (current) {
        setLocalNotice(
          "This task could not be refreshed. Cached conversation remains available.",
        );
      }
    });
    return () => {
      current = false;
      void owner.unwatchTask(taskId);
    };
  }, [selected?.hostPublicId, selected?.taskId, ownerSessionForWatch]);

  const openTask = (ref: ScopedHostTaskRef) => {
    setRouteUnavailable(false);
    setLocalNotice(null);
    selectedRef.current = ref;
    rememberSelectedTask(ref.hostPublicId, ref.taskId);
    pushNativeTaskRoute(ref);
    setSelected(ref);
  };

  const goBack = () => {
    selectedRef.current = null;
    pushNativeTaskRoute(null);
    setSelected(null);
    setRouteUnavailable(false);
    setLocalNotice(null);
  };

  useEffect(() => {
    if (!selected || selectedTask?.lifecycle !== "deleted") return;
    if (!selectedRef.current || scopedHostTaskKey(selectedRef.current) !== selectedKey) return;
    // A receipt is not deletion truth; retire only after this exact owner's
    // canonical projection confirms it. Never close a different focused task.
    if (readRememberedTask(selected.hostPublicId) === selected.taskId) {
      try {
        window.localStorage.removeItem(`${LAST_SELECTED_TASK_KEY_PREFIX}${selected.hostPublicId}`);
      } catch { /* Cache is optional; canonical deletion still closes the view. */ }
    }
    selectedRef.current = null;
    writeNativeTaskRoute(null, true);
    setSelected(null);
    setLocalNotice(null);
  }, [selectedKey, selectedTask?.lifecycle]);

  const updateDraft = (text: string) => {
    if (!selected) return;
    const owner = resolveSession(selected.hostPublicId);
    if (!owner) return;
    const key = scopedHostTaskKey(selected);
    const taskId = selected.taskId;
    const nextVersion = (draftVersionByKeyRef.current.get(key) ?? 0) + 1;
    draftVersionByKeyRef.current.set(key, nextVersion);
    draftTextByKeyRef.current.set(key, text);
    setComposer({ ownerKey: key, text });
    void owner.setDraft(taskId, text).catch(() => {
      if (
        selectedRef.current &&
        scopedHostTaskKey(selectedRef.current) === key &&
        draftVersionByKeyRef.current.get(key) === nextVersion
      ) {
        setLocalNotice("Draft could not be saved on this device. Keep this text open.");
      }
    });
  };

  const runMutation = async (
    ref: ScopedHostTaskRef,
    mutation: NativeTaskMutation,
  ): Promise<boolean> => {
    const owner = resolveSession(ref.hostPublicId);
    if (!owner) return false;
    const key = scopedHostTaskKey(ref);
    if (
      sendingIdentityByKeyRef.current.has(key) ||
      mutatingIdentityByKeyRef.current.has(key)
    ) {
      return false;
    }
    const mutateIdentity = ++nextMutateIdentityRef.current;
    mutatingIdentityByKeyRef.current.set(key, mutateIdentity);
    setMutatingByKey((current) => new Map(current).set(key, mutateIdentity));
    setLocalNotice(null);
    try {
      const result = await owner.mutateTask(ref.taskId, mutation);
      if (
        selectedRef.current &&
        scopedHostTaskKey(selectedRef.current) === key
      ) {
        if (!result.ok) {
          setLocalNotice(mutationFailureNotice(result.reason, mutation));
        }
      }
      return result.ok;
    } catch {
      if (
        selectedRef.current &&
        scopedHostTaskKey(selectedRef.current) === key
      ) {
        setLocalNotice(
          "Task action status is unknown. The original command remains in this device's outbox.",
        );
      }
      return false;
    } finally {
      if (mutatingIdentityByKeyRef.current.get(key) === mutateIdentity) {
        mutatingIdentityByKeyRef.current.delete(key);
      }
      setMutatingByKey((current) => {
        if (current.get(key) !== mutateIdentity) return current;
        const next = new Map(current);
        next.delete(key);
        return next;
      });
    }
  };

  const send = async () => {
    if (
      !selected ||
      !canSend ||
      sendingIdentityByKeyRef.current.has(scopedHostTaskKey(selected)) ||
      mutatingIdentityByKeyRef.current.has(scopedHostTaskKey(selected))
    ) {
      return;
    }
    const owner = resolveSession(selected.hostPublicId);
    if (!owner) return;
    const ref = selected;
    const key = scopedHostTaskKey(ref);
    // Submit snapshot must match the exact owner-tagged composer, never a
    // stale global string from another host/task.
    if (composer.ownerKey !== key) return;
    const textAtSend = composer.text;
    const draftVersionAtSend = draftVersionByKeyRef.current.get(key) ?? 0;
    const sendIdentity = ++nextSendIdentityRef.current;
    sendingIdentityByKeyRef.current.set(key, sendIdentity);
    setSendingByKey((current) => new Map(current).set(key, sendIdentity));
    setLocalNotice(null);
    try {
      const result = await owner.sendText(ref.taskId, textAtSend);
      if (!result.ok) {
        if (
          selectedRef.current &&
          scopedHostTaskKey(selectedRef.current) === key
        ) {
          setLocalNotice(
            result.reason === "transport_uncertain" ||
              result.reason === "reconciliation_required"
              ? "Delivery is not confirmed yet. Your message is retained on this device; checking its original receipt before retrying."
              : "Message was not accepted. Your draft is unchanged.",
          );
        }
        return;
      }
      if ((draftVersionByKeyRef.current.get(key) ?? 0) === draftVersionAtSend) {
        draftTextByKeyRef.current.set(key, "");
        void owner.setDraft(ref.taskId, "").catch(() => {
          if (
            selectedRef.current &&
            scopedHostTaskKey(selectedRef.current) === key &&
            (draftVersionByKeyRef.current.get(key) ?? 0) === draftVersionAtSend
          ) {
            setLocalNotice(
              "Message accepted, but the device draft could not be cleared yet.",
            );
          }
        });
        if (
          selectedRef.current &&
          scopedHostTaskKey(selectedRef.current) === key &&
          composer.ownerKey === key &&
          composer.text === textAtSend
        ) {
          setComposer({ ownerKey: key, text: "" });
        }
      }
    } catch {
      if (
        selectedRef.current &&
        scopedHostTaskKey(selectedRef.current) === key
      ) {
        setLocalNotice(
          "Message status is unknown. It remains in this device's outbox; do not resend yet.",
        );
      }
    } finally {
      if (sendingIdentityByKeyRef.current.get(key) === sendIdentity) {
        sendingIdentityByKeyRef.current.delete(key);
      }
      setSendingByKey((current) => {
        if (current.get(key) !== sendIdentity) return current;
        const next = new Map(current);
        next.delete(key);
        return next;
      });
    }
  };

  if (routeUnavailable || selectedOwnerMissing) {
    return (
      <main className="dm-native-remote">
        <header className="dm-native-remote__conversation-header">
          <button
            aria-label="Back to tasks"
            className="dm-native-remote__icon-button"
            onClick={goBack}
            type="button"
          >
            <ArrowLeft aria-hidden="true" size={20} />
          </button>
          <div className="dm-native-remote__conversation-title">
            <h1>Host unavailable</h1>
            <p>This conversation owner is not in the configured fleet.</p>
          </div>
        </header>
      </main>
    );
  }

  if (selected) {
    const activeSession = resolveSession(selected.hostPublicId);
    if (!activeSession) {
      return (
        <main className="dm-native-remote">
          <header className="dm-native-remote__conversation-header">
            <button
              aria-label="Back to tasks"
              className="dm-native-remote__icon-button"
              onClick={goBack}
              type="button"
            >
              <ArrowLeft aria-hidden="true" size={20} />
            </button>
            <div className="dm-native-remote__conversation-title">
              <h1>Host unavailable</h1>
              <p>This conversation owner is not in the configured fleet.</p>
            </div>
          </header>
        </main>
      );
    }
    return (
      <NativeConversationScreen
        key={scopedHostTaskKey(selected)}
        session={activeSession}
        connectionStatus={ownerView?.connectionStatus ?? "idle"}
        conversation={selectedConversation?.facts ?? []}
        hostLabel={
          showHostBadge
            ? hostLabelById.get(selected.hostPublicId) ?? hostLabel
            : hostLabel
        }
        localNotice={localNotice ?? ownerEntry?.notice ?? ownerView?.lastError ?? null}
        mutating={isSelectedMutating}
        onBack={goBack}
        onDraftChange={updateDraft}
        onMutate={(mutation) => {
          const frozen = selected;
          if (!frozen) return Promise.resolve(false);
          return runMutation(frozen, mutation);
        }}
        onSend={() => void send()}
        outbox={selectedOutbox}
        syncStatus={ownerView?.syncStatus ?? "cold"}
        task={selectedTask}
        taskId={selected.taskId}
        value={visibleComposer}
        canSend={canSend}
      />
    );
  }

  return (
    <main className="dm-native-remote" data-host-public-id={hostPublicId}>
      <header className="dm-native-remote__inbox-header">
        <div className="dm-native-remote__brand">
          <img src="/icons/devmanager-192.png" alt="" width={32} height={32} />
          <div>
            <h1>DevManager</h1>
            <p className="dm-native-remote__eyebrow">
              {showHostBadge
                ? `${hosts.length} hosts`
                : hostLabel ?? "Remote tasks"}
            </p>
          </div>
        </div>
        <div className="dm-native-remote__header-actions">
          <button
            aria-expanded={hostsOpen}
            aria-label="Host status"
            className="dm-native-remote__icon-button"
            onClick={() => {
              setHostsOpen((open) => !open);
              setArchiveOpen(false);
            }}
            type="button"
          >
            <Server aria-hidden="true" size={18} />
          </button>
          <button
            aria-expanded={archiveOpen}
            aria-label={archiveOpen ? "Close archived tasks" : "Show archived tasks"}
            className="dm-native-remote__icon-button"
            onClick={() => {
              setArchiveOpen((open) => !open);
              setHostsOpen(false);
            }}
            type="button"
          >
            <Archive aria-hidden="true" size={18} />
          </button>
          <button
            aria-expanded={searchOpen}
            aria-label={searchOpen ? "Close task search" : "Search tasks"}
            className="dm-native-remote__icon-button"
            onClick={() => setSearchOpen((open) => !open)}
            type="button"
          >
            <Search aria-hidden="true" size={19} />
          </button>
        </div>
      </header>

      {searchOpen ? (
        <div className="dm-native-remote__search-wrap">
          <Search aria-hidden="true" size={16} />
          <input
            aria-label="Search tasks"
            autoFocus
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Search task titles"
            type="search"
            value={search}
          />
        </div>
      ) : null}

      {hostsOpen ? (
        <HostStatusPanel
          hosts={hosts}
          onRetryHost={onRetryHost}
          onSubmitPairGrant={onSubmitPairGrant}
          onShowPagePairing={onShowPagePairing}
        />
      ) : null}

      {!showHostBadge ? (
        <ConnectionNotice
          connectionStatus={pageView.connectionStatus}
          syncStatus={pageView.syncStatus}
        />
      ) : null}

      {!showHostBadge && pageView.lastError ? (
        <p className="dm-native-remote__notice" role="status">
          <CircleAlert aria-hidden="true" size={16} />
          {pageView.lastError}
        </p>
      ) : null}

      {archiveOpen ? (
        <section aria-label="Archived tasks" className="dm-native-remote__task-list">
          {archivedTasks.map((task) => (
            <TaskRow
              key={scopedHostTaskKey({
                hostPublicId: task.hostPublicId,
                taskId: task.taskId,
              })}
              hostBadge={showHostBadge ? task.hostLabel : null}
              onOpen={() =>
                openTask({
                  hostPublicId: task.hostPublicId,
                  taskId: task.taskId,
                })
              }
              task={task}
            />
          ))}
          {archivedTasks.length === 0 ? (
            <p className="dm-native-remote__empty">No archived tasks.</p>
          ) : null}
        </section>
      ) : (
        <section aria-label="Task inbox" className="dm-native-remote__task-list">
          {inboxTasks.map((task) => (
            <TaskRow
              key={scopedHostTaskKey({
                hostPublicId: task.hostPublicId,
                taskId: task.taskId,
              })}
              hostBadge={showHostBadge ? task.hostLabel : null}
              onOpen={() =>
                openTask({ hostPublicId: task.hostPublicId, taskId: task.taskId })
              }
              task={task}
            />
          ))}
          {inboxTasks.length === 0 ? (
            <p className="dm-native-remote__empty">No active cached tasks.</p>
          ) : null}
          {doneTasks.length ? (
            <details className="dm-native-remote__done" open>
              <summary>Done ({doneTasks.length})</summary>
              {doneTasks.map((task) => (
                <TaskRow
                  key={scopedHostTaskKey({
                    hostPublicId: task.hostPublicId,
                    taskId: task.taskId,
                  })}
                  hostBadge={showHostBadge ? task.hostLabel : null}
                  onOpen={() =>
                    openTask({
                      hostPublicId: task.hostPublicId,
                      taskId: task.taskId,
                    })
                  }
                  task={task}
                />
              ))}
            </details>
          ) : null}
        </section>
      )}
    </main>
  );
}

function HostStatusPanel({
  hosts,
  onRetryHost,
  onSubmitPairGrant,
  onShowPagePairing,
}: {
  hosts: readonly NativeFleetEntrySnapshot[];
  onRetryHost?: (hostPublicId: string) => void;
  onSubmitPairGrant?: (hostPublicId: string, grant: string) => void;
  onShowPagePairing?: () => void;
}) {
  const [grantByHost, setGrantByHost] = useState<Record<string, string>>({});
  return (
    <section
      aria-label="Host status"
      className="dm-native-remote__host-panel"
    >
      <p className="dm-native-remote__eyebrow">
        Pairing grants are one-time and never stored as credentials.
      </p>
      <ul className="dm-native-remote__host-list">
        {hosts.map((entry) => {
          const id = entry.descriptor.hostPublicId;
          const live = entry.authenticated;
          return (
            <li key={id} className="dm-native-remote__host-row">
              <div className="dm-native-remote__host-copy">
                <strong>{entry.descriptor.label}</strong>
                <small>
                  {live
                    ? "Live"
                    : entry.pairingState === "pairing_required"
                      ? "Pairing required"
                      : entry.pairingState === "held"
                        ? "Held"
                        : entry.transportAttached
                          ? "Connected · awaiting auth"
                          : entry.view.connectionStatus === "ready"
                            ? "Syncing"
                            : "Unavailable"}
                  {entry.notice ? ` · ${entry.notice}` : ""}
                </small>
              </div>
              <div className="dm-native-remote__host-actions">
                {entry.descriptor.isPageHost &&
                entry.pairingState === "pairing_required" ? (
                  <button
                    type="button"
                    onClick={() => onShowPagePairing?.()}
                  >
                    Pair
                  </button>
                ) : null}
                {!entry.descriptor.isPageHost && !entry.authenticated ? (
                  <form
                    className="dm-native-remote__pair-form"
                    onSubmit={(event) => {
                      event.preventDefault();
                      const grant = (grantByHost[id] ?? "").trim();
                      if (!grant || !onSubmitPairGrant) return;
                      onSubmitPairGrant(id, grant);
                      setGrantByHost((current) => {
                        const next = { ...current };
                        delete next[id];
                        return next;
                      });
                    }}
                  >
                    <input
                      aria-label={`Pairing grant for ${entry.descriptor.label}`}
                      autoComplete="off"
                      onChange={(event) =>
                        setGrantByHost((current) => ({
                          ...current,
                          [id]: event.target.value,
                        }))
                      }
                      placeholder="One-time grant"
                      type="text"
                      value={grantByHost[id] ?? ""}
                    />
                    <button type="submit">Pair</button>
                  </form>
                ) : null}
                <button
                  type="button"
                  onClick={() => onRetryHost?.(id)}
                  disabled={live || entry.pairingState === "held"}
                >
                  Retry
                </button>
              </div>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

function NativeConversationScreen({
  session,
  canSend,
  connectionStatus,
  conversation,
  hostLabel,
  localNotice,
  mutating,
  onBack,
  onDraftChange,
  onMutate,
  onSend,
  outbox,
  syncStatus,
  task,
  taskId,
  value,
}: {
  session: NativeHostSession;
  canSend: boolean;
  connectionStatus: NativeHostSessionView["connectionStatus"];
  conversation: readonly SemanticJournalFact[];
  hostLabel?: string;
  localNotice: string | null;
  mutating: boolean;
  onBack: () => void;
  onDraftChange: (text: string) => void;
  onMutate: (mutation: NativeTaskMutation) => Promise<boolean>;
  onSend: () => void;
  outbox: readonly { taskId: NativeUuid; status: string }[];
  syncStatus: NativeHostSessionView["syncStatus"];
  task: TaskMeta | null;
  taskId: NativeUuid;
  value: string;
}) {
  const timeline = useMemo(() => buildNativeTimeline(conversation), [conversation]);
  const uncertain = outbox.some((item) => item.status === "uncertain");
  const blocked = outbox.some((item) => item.status === "blocked_client_mismatch");
  const title = task?.title ?? "Task";
  const lifecycle = task?.lifecycle ?? null;
  const [terminalOpen, setTerminalOpen] = useState(false);
  const [menuOpen, setMenuOpen] = useState(false);
  const [renameOpen, setRenameOpen] = useState(false);
  const [renameValue, setRenameValue] = useState(title);
  const [deleteConfirm, setDeleteConfirm] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!menuOpen) return;
    const onPointer = (event: MouseEvent) => {
      if (!menuRef.current?.contains(event.target as Node)) {
        setMenuOpen(false);
        setRenameOpen(false);
        setDeleteConfirm(false);
      }
    };
    document.addEventListener("mousedown", onPointer);
    return () => document.removeEventListener("mousedown", onPointer);
  }, [menuOpen]);

  useEffect(() => {
    setRenameValue(title);
  }, [title]);

  const live =
    connectionStatus === "ready" && syncStatus === "live" && !mutating && outbox.length === 0;
  const canSettle = live && (lifecycle === "open" || lifecycle === "settled");
  const canRestore =
    live &&
    (lifecycle === "settled" ||
      lifecycle === "closing" ||
      lifecycle === "archived");
  const canRename =
    live && (lifecycle === "open" || lifecycle === "settled" || lifecycle === "closing");
  const canArchive =
    live && (lifecycle === "open" || lifecycle === "settled" || lifecycle === "closing");
  const canDelete = live && lifecycle === "archived";
  // Guidance only — closing/archived need Restore; Done may still SendNow.
  const requiresRestoreBeforeSend =
    lifecycle === "archived" || lifecycle === "closing";

  return (
    <main className="dm-native-remote dm-native-remote--conversation">
      <header className="dm-native-remote__conversation-header">
        <button
          aria-label="Back to tasks"
          className="dm-native-remote__icon-button"
          onClick={onBack}
          type="button"
        >
          <ArrowLeft aria-hidden="true" size={20} />
        </button>
        <div className="dm-native-remote__conversation-title">
          <h1>{title}</h1>
          {hostLabel ? <p>{hostLabel}</p> : null}
          {lifecycle === "closing" ? (
            <p className="dm-native-remote__lifecycle">Archiving in progress</p>
          ) : null}
          {lifecycle === "archived" ? (
            <p className="dm-native-remote__lifecycle">Archived · restore to write</p>
          ) : null}
          {lifecycle === "settled" ? (
            <p className="dm-native-remote__lifecycle">Done · send restores automatically</p>
          ) : null}
        </div>
        <div className="dm-native-remote__header-actions">
          <div className="dm-native-remote__menu" ref={menuRef}>
            <button
              aria-expanded={menuOpen}
              aria-haspopup="menu"
              aria-label="Task actions"
              className="dm-native-remote__icon-button"
              disabled={mutating}
              onClick={() => {
                setMenuOpen((open) => !open);
                setRenameOpen(false);
                setDeleteConfirm(false);
              }}
              type="button"
            >
              <MoreVertical aria-hidden="true" size={18} />
            </button>
            {menuOpen ? (
              <div
                className="dm-native-remote__menu-panel"
                role="menu"
                aria-label="Task actions"
              >
                {canSettle && lifecycle === "open" ? (
                  <button
                    role="menuitem"
                    type="button"
                    onClick={() => {
                      setMenuOpen(false);
                      void onMutate({ kind: "settle" });
                    }}
                  >
                    <Check aria-hidden="true" size={14} />
                    Done
                  </button>
                ) : null}
                {canRestore ? (
                  <button
                    role="menuitem"
                    type="button"
                    onClick={() => {
                      setMenuOpen(false);
                      void onMutate({ kind: "reopen" });
                    }}
                  >
                    <RotateCcw aria-hidden="true" size={14} />
                    Restore
                  </button>
                ) : null}
                {canRename ? (
                  <button
                    role="menuitem"
                    type="button"
                    onClick={() => {
                      setRenameOpen(true);
                      setDeleteConfirm(false);
                    }}
                  >
                    <Pencil aria-hidden="true" size={14} />
                    Rename
                  </button>
                ) : null}
                {canArchive && lifecycle !== "closing" ? (
                  <button
                    role="menuitem"
                    type="button"
                    onClick={() => {
                      setMenuOpen(false);
                      void onMutate({ kind: "begin_close" });
                    }}
                  >
                    <Archive aria-hidden="true" size={14} />
                    Archive
                  </button>
                ) : null}
                {canDelete ? (
                  <button
                    role="menuitem"
                    type="button"
                    className="dm-native-remote__menu-danger"
                    onClick={() => {
                      setDeleteConfirm(true);
                      setRenameOpen(false);
                    }}
                  >
                    <Trash2 aria-hidden="true" size={14} />
                    Delete
                  </button>
                ) : null}
                {renameOpen ? (
                  <form
                    className="dm-native-remote__rename-form"
                    onSubmit={(event) => {
                      event.preventDefault();
                      const next = renameValue.trim();
                      if (!next) return;
                      setMenuOpen(false);
                      setRenameOpen(false);
                      void onMutate({ kind: "rename", title: next });
                    }}
                  >
                    <label className="dm-native-remote__eyebrow" htmlFor="dm-native-rename">
                      New title
                    </label>
                    <input
                      id="dm-native-rename"
                      aria-label="New task title"
                      autoFocus
                      onChange={(event) => setRenameValue(event.target.value)}
                      type="text"
                      value={renameValue}
                    />
                    <button type="submit">Save title</button>
                  </form>
                ) : null}
                {deleteConfirm ? (
                  <div className="dm-native-remote__delete-confirm" role="group" aria-label="Confirm delete">
                    <p>Delete this archived task permanently?</p>
                    <button
                      type="button"
                      className="dm-native-remote__menu-danger"
                      onClick={() => {
                        setMenuOpen(false);
                        setDeleteConfirm(false);
                        void onMutate({ kind: "delete" });
                      }}
                    >
                      Confirm delete
                    </button>
                    <button
                      type="button"
                      onClick={() => setDeleteConfirm(false)}
                    >
                      Cancel
                    </button>
                  </div>
                ) : null}
              </div>
            ) : null}
          </div>
          <button
            aria-label={terminalOpen ? "Show conversation" : "Show terminal"}
            aria-pressed={terminalOpen}
            className="dm-native-remote__icon-button"
            onClick={() => setTerminalOpen((open) => !open)}
            type="button"
          >
            {terminalOpen ? (
              <MessageSquare aria-hidden="true" size={20} />
            ) : (
              <SquareTerminal aria-hidden="true" size={20} />
            )}
          </button>
        </div>
      </header>

      <ConnectionNotice connectionStatus={connectionStatus} syncStatus={syncStatus} />
      {localNotice ? (
        <p className="dm-native-remote__notice" role="status">
          <CircleAlert aria-hidden="true" size={16} />
          {localNotice}
        </p>
      ) : null}
      {uncertain || blocked ? (
        <p className="dm-native-remote__notice dm-native-remote__notice--caution" role="status">
          <CircleAlert aria-hidden="true" size={16} />
          {blocked
            ? "Sending is paused until this browser is re-authorized."
            : "Message delivery is uncertain. Waiting for host reconciliation."}
        </p>
      ) : null}

      {terminalOpen ? (
        <NativeTerminalScreen
          session={session}
          taskId={taskId}
          connectionStatus={connectionStatus}
        />
      ) : (
        <SemanticTimeline facts={conversation} items={timeline} taskId={taskId} />
      )}

      {!terminalOpen ? (
        <form
          className="dm-native-remote__composer"
          onSubmit={(event) => {
            event.preventDefault();
            onSend();
          }}
        >
          <textarea
            aria-label="Message"
            onChange={(event) => onDraftChange(event.target.value)}
            placeholder={
              requiresRestoreBeforeSend
                ? lifecycle === "closing"
                  ? "Archiving in progress — restore to write"
                  : "Archived — restore before sending"
                : lifecycle === "settled"
                  ? "Done — send restores this task"
                : connectionStatus === "ready" && syncStatus === "live"
                  ? "Message this task"
                  : "Draft stays editable until the host is live"
            }
            rows={1}
            value={value}
          />
          <button aria-label="Send" disabled={!canSend} type="submit">
            <Send aria-hidden="true" size={18} />
          </button>
        </form>
      ) : null}
    </main>
  );
}

function NativeTerminalScreen({
  session,
  taskId,
  connectionStatus,
}: {
  session: NativeHostSession;
  taskId: NativeUuid;
  connectionStatus: NativeHostSessionView["connectionStatus"];
}) {
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [keyPending, setKeyPending] = useState(false);
  const [keyNotice, setKeyNotice] = useState<string | null>(null);
  const keyInFlight = useRef(false);
  const active = useRef(true);
  useEffect(() => {
    active.current = true;
    return () => {
      active.current = false;
    };
  }, []);
  const pressKey = async (key: NativeTerminalKey) => {
    if (keyInFlight.current || connectionStatus !== "ready") return;
    keyInFlight.current = true;
    setKeyPending(true);
    setKeyNotice(null);
    try {
      const result = await session.sendTerminalKey(taskId, key);
      if (active.current && !result.ok) {
        setKeyNotice(
          result.reason === "transport_uncertain" ||
            result.reason === "reconciliation_required"
            ? "Key delivery is unconfirmed. Checking its original receipt; do not repeat it yet."
            : "Terminal key was not accepted. Check the host connection or pending approval.",
        );
      }
    } catch {
      if (active.current) setKeyNotice("Terminal key could not be confirmed.");
    } finally {
      keyInFlight.current = false;
      if (active.current) setKeyPending(false);
    }
  };
  useEffect(() => {
    let activeRefresh = true;
    let inFlight = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const refresh = async () => {
      if (!activeRefresh || inFlight || document.hidden || connectionStatus !== "ready")
        return;
      if (timer !== undefined) clearTimeout(timer);
      inFlight = true;
      try {
        const snapshot = await session.readTerminal(taskId);
        if (activeRefresh) {
          setText(snapshot.textLines.join("\n"));
          setError(null);
        }
      } catch (cause) {
        if (activeRefresh)
          setError(cause instanceof Error ? cause.message : "Terminal unavailable");
      } finally {
        inFlight = false;
        if (activeRefresh && !document.hidden)
          timer = setTimeout(() => void refresh(), 1000);
      }
    };
    const onVisible = () => {
      if (!document.hidden) void refresh();
    };
    document.addEventListener("visibilitychange", onVisible);
    void refresh();
    return () => {
      activeRefresh = false;
      clearTimeout(timer);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [session, taskId, connectionStatus]);
  return (
    <section className="dm-native-remote__terminal" aria-label="Terminal">
      <div className="dm-native-remote__terminal-keys" role="group" aria-label="Terminal keys">
        {(
          [
            ["up", "↑"],
            ["down", "↓"],
            ["enter", "Enter"],
            ["escape", "Esc"],
            ["interrupt", "Ctrl+C"],
          ] as const
        ).map(([key, label]) => (
          <button
            key={key}
            type="button"
            aria-label={`Terminal ${key}`}
            disabled={keyPending || connectionStatus !== "ready"}
            onClick={() => void pressKey(key)}
          >
            {label}
          </button>
        ))}
      </div>
      <p className="dm-native-remote__eyebrow">Live owner terminal · startup controls</p>
      {error ? <p role="status">{error}</p> : null}
      {keyNotice ? <p role="status">{keyNotice}</p> : null}
      <pre>{text || "Waiting for terminal output…"}</pre>
    </section>
  );
}

function SemanticTimeline({
  facts,
  items,
  taskId,
}: {
  facts: readonly SemanticJournalFact[];
  items: ReturnType<typeof buildNativeTimeline>;
  taskId: NativeUuid;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const firstOpenRef = useRef(true);
  const followsBottomRef = useRef(true);

  useEffect(() => {
    firstOpenRef.current = true;
    followsBottomRef.current = true;
  }, [taskId]);

  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (!element || (!firstOpenRef.current && !followsBottomRef.current)) {
      return;
    }
    element.scrollTop = element.scrollHeight;
    firstOpenRef.current = false;
  }, [facts, items, taskId]);

  return (
    <section
      aria-label="Conversation"
      className="dm-native-remote__timeline"
      onScroll={(event) => {
        const element = event.currentTarget;
        followsBottomRef.current =
          element.scrollHeight - element.scrollTop - element.clientHeight < 96;
      }}
      ref={scrollRef}
    >
      {items.length ? (
        items.map((item) => {
          switch (item.kind) {
            case "user":
              return (
                <article
                  className="dm-native-remote__message dm-native-remote__message--user"
                  key={item.id}
                >
                  <MarkdownMessage text={item.text} />
                </article>
              );
            case "assistant":
              return (
                <article className="dm-native-remote__message" key={item.id}>
                  {item.messages.map((message) => (
                    <MarkdownMessage key={message.id} text={message.text} />
                  ))}
                  {item.reasoning.length ? (
                    <details className="dm-native-remote__details">
                      <summary>Reasoning summary</summary>
                      {item.reasoning.map((text, index) => (
                        <MarkdownMessage
                          key={`${item.id}-reasoning-${index}`}
                          text={text}
                        />
                      ))}
                    </details>
                  ) : null}
                </article>
              );
            case "activity":
              return (
                <details className="dm-native-remote__activity" key={item.id}>
                  <summary>{item.title}</summary>
                  <ul>
                    {item.details.map((detail, index) => (
                      <li key={`${item.id}-${index}`}>{detail}</li>
                    ))}
                  </ul>
                </details>
              );
            case "question":
              return (
                <article className="dm-native-remote__question" key={item.id}>
                  <p>{item.prompt}</p>
                  {item.options.length ? (
                    <ul>
                      {item.options.map((option) => (
                        <li key={option}>{option}</li>
                      ))}
                    </ul>
                  ) : null}
                </article>
              );
            case "error":
              return (
                <article className="dm-native-remote__error" key={item.id}>
                  <CircleAlert aria-hidden="true" size={16} />
                  <span>{item.message}</span>
                </article>
              );
          }
        })
      ) : (
        <p className="dm-native-remote__empty">No cached conversation yet.</p>
      )}
    </section>
  );
}

function TaskRow({
  onOpen,
  task,
  hostBadge,
}: {
  onOpen: () => void;
  task: TaskMeta;
  hostBadge?: string | null;
}) {
  const metadata = [
    isClosingTask(task) ? "Archiving…" : null,
    task.activity,
    task.attention,
    task.connectivity,
  ].filter((value): value is string => Boolean(value));
  return (
    <button className="dm-native-remote__task-row" onClick={onOpen} type="button">
      <span className="dm-native-remote__task-copy">
        <strong>{task.title ?? "Task"}</strong>
        <small>
          {[hostBadge, ...metadata].filter(Boolean).join(" · ")}
        </small>
      </span>
      <ChevronRight aria-hidden="true" size={18} />
    </button>
  );
}

function ConnectionNotice({
  connectionStatus,
  syncStatus,
}: {
  connectionStatus: NativeHostSessionView["connectionStatus"];
  syncStatus: NativeHostSessionView["syncStatus"];
}) {
  if (connectionStatus === "ready" && syncStatus === "live") {
    return null;
  }
  return (
    <p className="dm-native-remote__connection" role="status">
      <WifiOff aria-hidden="true" size={15} />
      {connectionStatus === "ready"
        ? "Syncing host changes. Cached content is available."
        : "Host unavailable. Showing cached content."}
    </p>
  );
}

function isDoneTask(task: TaskMeta): boolean {
  return task.lifecycle === "settled";
}

function isArchivedTask(task: TaskMeta): boolean {
  return task.lifecycle === "archived";
}

function isClosingTask(task: TaskMeta): boolean {
  return task.lifecycle === "closing";
}

function mutationFailureNotice(
  reason: NativeSendFailure,
  mutation: NativeTaskMutation,
): string {
  if (reason === "invalid_lifecycle") {
    if (mutation.kind === "delete") {
      return "Delete is only available after the task is archived.";
    }
    return "That action is not available for this task's current state.";
  }
  if (reason === "storage_failure") {
    return "Could not save the action status on this device. Check the retained command before trying again.";
  }
  if (reason === "rejected") {
    return "The host rejected that action. Lifecycle is unchanged.";
  }
  if (reason === "transport_uncertain" || reason === "reconciliation_required") {
    return "Action status is uncertain. The original command ID is retained; do not repeat it yet.";
  }
  return "Task action was not accepted. Lifecycle is unchanged.";
}

function compareMergedTasks(left: MergedTaskRow, right: MergedTaskRow): number {
  return (
    right.updatedAtMs - left.updatedAtMs ||
    left.hostPublicId.localeCompare(right.hostPublicId) ||
    left.taskId.localeCompare(right.taskId)
  );
}

type RouteParse =
  | { kind: "none"; ref: null }
  | { kind: "selected"; ref: ScopedHostTaskRef }
  | { kind: "unavailable"; ref: null };

/**
 * `/tasks/:hostPublicId/:taskId` is canonical.
 * Legacy `/tasks/:taskId` migrates only to the authoritative page host.
 * Unknown owner routes are unavailable and must not dial.
 */
function readNativeTaskRoute(pageHostPublicId: string): RouteParse {
  if (typeof window === "undefined") return { kind: "none", ref: null };
  const parts = window.location.pathname.split("/").filter(Boolean);
  if (parts[0] !== "tasks") return { kind: "none", ref: null };
  if (parts.length === 1) return { kind: "none", ref: null };
  if (parts.length === 2) {
    try {
      const taskId = protocolUuid(decodeURIComponent(parts[1] ?? ""));
      if (!taskId) return { kind: "none", ref: null };
      return {
        kind: "selected",
        ref: { hostPublicId: pageHostPublicId, taskId },
      };
    } catch {
      return { kind: "none", ref: null };
    }
  }
  if (parts.length === 3) {
    const ref = scopeHostTask(
      decodeURIComponent(parts[1] ?? ""),
      decodeURIComponent(parts[2] ?? ""),
    );
    if (!ref) return { kind: "unavailable", ref: null };
    return { kind: "selected", ref };
  }
  return { kind: "unavailable", ref: null };
}

function readRememberedTask(hostPublicId: string): NativeUuid | null {
  const host = protocolUuid(hostPublicId);
  if (!host) return null;
  try {
    return protocolUuid(
      globalThis.localStorage?.getItem(`${LAST_SELECTED_TASK_KEY_PREFIX}${host}`),
    );
  } catch {
    return null;
  }
}

function rememberSelectedTask(hostPublicId: string, taskId: NativeUuid): void {
  const host = protocolUuid(hostPublicId);
  const task = protocolUuid(taskId);
  if (!host || !task) return;
  try {
    globalThis.localStorage?.setItem(
      `${LAST_SELECTED_TASK_KEY_PREFIX}${host}`,
      task,
    );
  } catch {
    // Navigation remains available when storage is unavailable.
  }
}

function pushNativeTaskRoute(ref: ScopedHostTaskRef | null): void {
  writeNativeTaskRoute(ref, false);
}

function replaceNativeTaskRoute(ref: ScopedHostTaskRef): void {
  writeNativeTaskRoute(ref, true);
}

function writeNativeTaskRoute(
  ref: ScopedHostTaskRef | null,
  replace: boolean,
): void {
  if (typeof window === "undefined") return;
  const nextPath = ref
    ? `/tasks/${encodeURIComponent(ref.hostPublicId)}/${encodeURIComponent(ref.taskId)}`
    : "/tasks";
  if (window.location.pathname === nextPath && !window.location.search) return;
  if (replace) window.history.replaceState(null, "", nextPath);
  else window.history.pushState(null, "", nextPath);
  window.dispatchEvent(new PopStateEvent("popstate"));
}
