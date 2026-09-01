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
  Plus,
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
import type {
  NativeConfigSnapshotView,
  NativeAccessMode,
  NativeProviderKind,
  NativeProviderLaunchOptions,
  NativeProviderModel,
  NativeReasoningEffort,
  NativeTerminalKey,
  NativeUuid,
  SemanticJournalFact,
} from "./nativeProtocol";
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
const LAST_PROVIDER_KEY_PREFIX = "devmanager.connect.native.last-provider.v1:";
const LAST_OPTIONS_KEY_PREFIX = "devmanager.connect.native.last-options.v1:";

type NewTaskProject = {
  hostPublicId: NativeUuid;
  hostLabel: string;
  projectId: NativeUuid;
  projectLabel: string;
  provider: NativeProviderKind;
  providers: NativeProviderKind[];
  launchOptions: NativeProviderLaunchOptions;
};

function rememberedLaunchOptions(
  hostPublicId: string,
  provider: NativeProviderKind,
): NativeProviderLaunchOptions {
  const fallback: NativeProviderLaunchOptions = {
    model: "provider_default",
    reasoningEffort: "provider_default",
    access: "full_access",
  };
  try {
    const raw = window.localStorage.getItem(
      `${LAST_OPTIONS_KEY_PREFIX}${hostPublicId}:${provider}`,
    );
    if (!raw) return fallback;
    const parsed = JSON.parse(raw) as Partial<NativeProviderLaunchOptions>;
    const models: NativeProviderModel[] = provider === "codex"
      ? ["provider_default", "codex_sol", "codex_terra", "codex_luna"]
      : ["provider_default", "claude_opus", "claude_sonnet", "claude_haiku"];
    const efforts: NativeReasoningEffort[] = provider === "codex"
      ? ["provider_default", "low", "medium", "high", "extra_high", "max", "ultra"]
      : ["provider_default", "low", "medium", "high"];
    const access: NativeAccessMode[] = ["full_access", "workspace_write", "read_only"];
    if (!models.includes(parsed.model as NativeProviderModel) ||
        !efforts.includes(parsed.reasoningEffort as NativeReasoningEffort) ||
        !access.includes(parsed.access as NativeAccessMode)) {
      return fallback;
    }
    return parsed as NativeProviderLaunchOptions;
  } catch {
    return fallback;
  }
}

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
  const [newTaskProjects, setNewTaskProjects] = useState<NewTaskProject[] | null>(null);
  const [newTaskLoading, setNewTaskLoading] = useState(false);
  const [newTaskDraft, setNewTaskDraft] = useState<NewTaskProject | null>(null);
  const [newTaskNotice, setNewTaskNotice] = useState<string | null>(null);
  const [newTaskSubmitting, setNewTaskSubmitting] = useState(false);
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
  const hasLiveHost = hosts.some((entry) => entry.authenticated);

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
          setLocalNotice(sendFailureNotice(result.reason));
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

  const openNewTask = async () => {
    if (newTaskLoading) return;
    setNewTaskLoading(true);
    setNewTaskNotice(null);
    const rows: NewTaskProject[] = [];
    await Promise.all(hosts.map(async (entry) => {
      if (!entry.authenticated) return;
      const owner = resolveSession(entry.descriptor.hostPublicId);
      if (!owner) return;
      try {
        const config: NativeConfigSnapshotView = await owner.readConfigSnapshot();
        // `commandConfigured` means the user supplied an executable override;
        // false still uses the provider's discovered/default CLI. Treating it
        // as availability made every ordinary installation look provider-less.
        const available = new Set(config.providers.map((provider) => provider.provider));
        const remembered = window.localStorage.getItem(`${LAST_PROVIDER_KEY_PREFIX}${entry.descriptor.hostPublicId}`);
        const provider: NativeProviderKind = (remembered === "claude" || remembered === "claude_code") && available.has("claude")
          ? "claude"
          : remembered === "codex" && available.has("codex")
            ? "codex"
            : available.has("codex") ? "codex" : "claude";
        const providers: NativeProviderKind[] = [
          ...(available.has("codex") ? ["codex" as const] : []),
          ...(available.has("claude") ? ["claude" as const] : []),
        ];
        const launchOptions = rememberedLaunchOptions(
          entry.descriptor.hostPublicId,
          provider,
        );
        if (providers.length === 0) return;
        for (const project of config.projects) {
          if (!project.rootConfigured || !project.workspaceId) continue;
          rows.push({
            hostPublicId: entry.descriptor.hostPublicId,
            hostLabel: entry.descriptor.label,
            projectId: project.workspaceId,
            projectLabel: project.label,
            provider,
            providers,
            launchOptions,
          });
        }
      } catch {
        // Other live hosts remain available; the empty state explains failure.
      }
    }));
    rows.sort((left, right) => left.hostLabel.localeCompare(right.hostLabel) ||
      left.projectLabel.localeCompare(right.projectLabel));
    setNewTaskProjects(rows);
    setNewTaskLoading(false);
  };

  const submitNewTask = async (text: string) => {
    const draft = newTaskDraft;
    if (!draft || newTaskSubmitting || !text.trim()) return;
    const owner = resolveSession(draft.hostPublicId);
    if (!owner) {
      setNewTaskNotice("The selected host is no longer available.");
      return;
    }
    setNewTaskSubmitting(true);
    setNewTaskNotice(null);
    window.localStorage.setItem(`${LAST_PROVIDER_KEY_PREFIX}${draft.hostPublicId}`, draft.provider);
    window.localStorage.setItem(
      `${LAST_OPTIONS_KEY_PREFIX}${draft.hostPublicId}:${draft.provider}`,
      JSON.stringify(draft.launchOptions),
    );
    try {
      const result = await owner.createTaskAndSend({
        projectId: draft.projectId,
        provider: draft.provider,
        launchOptions: draft.launchOptions,
        text,
      });
      if (result.ok) {
        const next = { hostPublicId: draft.hostPublicId, taskId: result.taskId };
        setNewTaskDraft(null);
        setNewTaskProjects(null);
        openTask(next);
        return;
      }
      if (result.taskId) {
        await owner.setDraft(result.taskId, text).catch(() => undefined);
        setNewTaskDraft(null);
        setNewTaskProjects(null);
        setLocalNotice("The task was created, but its provider was not ready. Your message remains as a draft.");
        openTask({ hostPublicId: draft.hostPublicId, taskId: result.taskId });
      } else {
        setNewTaskNotice(sendFailureNotice(result.reason));
      }
    } catch {
      setNewTaskNotice("Task creation could not be confirmed. Check the host before retrying.");
    } finally {
      setNewTaskSubmitting(false);
    }
  };

  if (newTaskDraft) {
    return (
      <NativeNewTaskScreen
        draft={newTaskDraft}
        notice={newTaskNotice}
        onBack={() => { setNewTaskDraft(null); setNewTaskProjects(null); setNewTaskNotice(null); }}
        onProviderChange={(provider) => setNewTaskDraft((current) => current ? {
          ...current,
          provider,
          launchOptions: rememberedLaunchOptions(current.hostPublicId, provider),
        } : null)}
        onLaunchOptionsChange={(launchOptions) => setNewTaskDraft((current) => current ? { ...current, launchOptions } : null)}
        onSend={submitNewTask}
        submitting={newTaskSubmitting}
      />
    );
  }

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
            aria-label="New task"
            className="dm-native-remote__new-task-button"
            disabled={newTaskLoading || !hasLiveHost}
            onClick={() => void openNewTask()}
            title={hasLiveHost ? "Create a task" : "Waiting for a live host"}
            type="button"
          >
            <Plus aria-hidden="true" size={17} />
            {newTaskLoading ? "Loading…" : "New task"}
          </button>
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
            onClick={() =>
              setSearchOpen((open) => {
                if (open) setSearch("");
                return !open;
              })
            }
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

      {newTaskProjects ? (
        <section aria-label="Choose a project" className="dm-native-remote__project-picker">
          <div className="dm-native-remote__project-picker-heading">
            <div><strong>Choose a project</strong><small>The task is saved only after your first message.</small></div>
            <button aria-label="Close new task" onClick={() => setNewTaskProjects(null)} type="button">×</button>
          </div>
          {newTaskProjects.map((project) => (
            <button
              key={`${project.hostPublicId}:${project.projectId}`}
              onClick={() => { setNewTaskDraft(project); setNewTaskNotice(null); }}
              type="button"
            >
              <strong>{project.projectLabel}</strong>
              {showHostBadge ? <small>{project.hostLabel}</small> : null}
            </button>
          ))}
          {newTaskProjects.length === 0 ? <p>No configured project is available on a live host.</p> : null}
        </section>
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

function NativeNewTaskScreen({
  draft,
  notice,
  onBack,
  onProviderChange,
  onLaunchOptionsChange,
  onSend,
  submitting,
}: {
  draft: NewTaskProject;
  notice: string | null;
  onBack: () => void;
  onProviderChange: (provider: NativeProviderKind) => void;
  onLaunchOptionsChange: (options: NativeProviderLaunchOptions) => void;
  onSend: (text: string) => Promise<void>;
  submitting: boolean;
}) {
  const [text, setText] = useState("");
  return (
    <main className="dm-native-remote dm-native-remote--conversation dm-native-remote--new-task">
      <header className="dm-native-remote__conversation-header">
        <button aria-label="Cancel new task" className="dm-native-remote__icon-button" onClick={onBack} type="button">
          <ArrowLeft aria-hidden="true" size={20} />
        </button>
        <div className="dm-native-remote__conversation-title">
          <h1>New task</h1>
          <p>{draft.projectLabel}{draft.hostLabel ? ` · ${draft.hostLabel}` : ""}</p>
        </div>
        <span />
      </header>
      {notice ? <p className="dm-native-remote__notice" role="status"><CircleAlert aria-hidden="true" size={16} />{notice}</p> : null}
      <section className="dm-native-remote__new-task-empty" aria-label="Unsaved new task">
        <img src="/icons/devmanager-192.png" alt="" width={54} height={54} />
        <h2>What should we build?</h2>
        <p>This shell is local to your browser. It becomes a real task on {draft.hostLabel} only when you send the first message.</p>
      </section>
      <form className="dm-native-remote__composer dm-native-remote__composer--new" onSubmit={(event) => {
        event.preventDefault();
        void onSend(text);
      }}>
        <textarea
          aria-label="Message"
          autoFocus
          disabled={submitting}
          onChange={(event) => setText(event.target.value)}
          placeholder="Describe the task"
          rows={2}
          value={text}
        />
        <div className="dm-native-remote__composer-options">
          <label>
            <span className="sr-only">Provider</span>
            <select
              aria-label="Provider"
              disabled={submitting || draft.providers.length < 2}
              onChange={(event) => onProviderChange(event.target.value as NativeProviderKind)}
              value={draft.provider}
            >
              {draft.providers.includes("codex") ? <option value="codex">Codex</option> : null}
              {draft.providers.includes("claude") ? <option value="claude">Claude Code</option> : null}
            </select>
          </label>
          <label>
            <span className="sr-only">Model</span>
            <select
              aria-label="Model"
              disabled={submitting}
              onChange={(event) => onLaunchOptionsChange({
                ...draft.launchOptions,
                model: event.target.value as NativeProviderModel,
              })}
              value={draft.launchOptions.model}
            >
              <option value="provider_default">Default model</option>
              {draft.provider === "codex" ? (
                <>
                  <option value="codex_sol">GPT-5.6 Sol</option>
                  <option value="codex_terra">GPT-5.6 Terra</option>
                  <option value="codex_luna">GPT-5.6 Luna</option>
                </>
              ) : (
                <>
                  <option value="claude_opus">Claude Opus</option>
                  <option value="claude_sonnet">Claude Sonnet</option>
                  <option value="claude_haiku">Claude Haiku</option>
                </>
              )}
            </select>
          </label>
          <label>
            <span className="sr-only">Thinking</span>
            <select
              aria-label="Thinking"
              disabled={submitting}
              onChange={(event) => onLaunchOptionsChange({
                ...draft.launchOptions,
                reasoningEffort: event.target.value as NativeReasoningEffort,
              })}
              value={draft.launchOptions.reasoningEffort}
            >
              <option value="provider_default">Default thinking</option>
              <option value="low">Low</option>
              <option value="medium">Medium</option>
              <option value="high">High</option>
              {draft.provider === "codex" ? <option value="extra_high">Extra high</option> : null}
              {draft.provider === "codex" ? <option value="max">Max</option> : null}
              {draft.provider === "codex" ? <option value="ultra">Ultra</option> : null}
            </select>
          </label>
          <label>
            <span className="sr-only">Access</span>
            <select
              aria-label="Access"
              disabled={submitting}
              onChange={(event) => onLaunchOptionsChange({
                ...draft.launchOptions,
                access: event.target.value as NativeAccessMode,
              })}
              value={draft.launchOptions.access}
            >
              <option value="full_access">Full access</option>
              <option value="workspace_write">Workspace write</option>
              <option value="read_only">Read only</option>
            </select>
          </label>
          <span>Main workspace</span>
        </div>
        <button aria-label="Send" disabled={submitting || !text.trim()} type="submit">
          <Send aria-hidden="true" size={18} />
        </button>
      </form>
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
                {!live && entry.view.lastError ? (
                  <small className="dm-native-remote__host-error">
                    {entry.view.lastError}
                  </small>
                ) : null}
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
  const [questionPending, setQuestionPending] = useState(false);
  const [questionNotice, setQuestionNotice] = useState<string | null>(null);
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
  const answerableQuestionId = task?.attention === "needs_answer"
    ? [...timeline].reverse().find((item) => item.kind === "question")?.id ?? null
    : null;
  const answerQuestion = async (answer: string) => {
    if (questionPending || !answer.trim()) return false;
    setQuestionPending(true);
    setQuestionNotice(null);
    try {
      const result = await session.answerQuestion(taskId, answer.trim());
      if (!result.ok) {
        setQuestionNotice(sendFailureNotice(result.reason));
        return false;
      }
      return true;
    } catch {
      setQuestionNotice("The answer could not be confirmed. Check the task before retrying.");
      return false;
    } finally {
      setQuestionPending(false);
    }
  };

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
      {questionNotice ? (
        <p className="dm-native-remote__notice" role="status">
          <CircleAlert aria-hidden="true" size={16} />
          {questionNotice}
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
        <SemanticTimeline
          answerableQuestionId={answerableQuestionId}
          answering={questionPending}
          facts={conversation}
          items={timeline}
          onAnswerQuestion={answerQuestion}
          taskId={taskId}
        />
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
  const [terminalAvailable, setTerminalAvailable] = useState(false);
  const [keyPending, setKeyPending] = useState(false);
  const [keyNotice, setKeyNotice] = useState<string | null>(null);
  const [terminalInput, setTerminalInput] = useState("");
  const keyInFlight = useRef(false);
  const active = useRef(true);
  useEffect(() => {
    active.current = true;
    return () => {
      active.current = false;
    };
  }, []);
  const pressKey = async (key: NativeTerminalKey) => {
    if (keyInFlight.current || connectionStatus !== "ready" || !terminalAvailable) return;
    keyInFlight.current = true;
    setKeyPending(true);
    setKeyNotice(null);
    try {
      const result = await session.sendTerminalKey(taskId, key);
      if (active.current && !result.ok) {
        setKeyNotice(sendFailureNotice(result.reason));
      }
    } catch {
      if (active.current) setKeyNotice("Terminal key could not be confirmed.");
    } finally {
      keyInFlight.current = false;
      if (active.current) setKeyPending(false);
    }
  };
  const submitTerminalInput = async (event: React.FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (
      keyInFlight.current ||
      connectionStatus !== "ready" ||
      !terminalAvailable ||
      terminalInput.length === 0
    ) return;
    keyInFlight.current = true;
    setKeyPending(true);
    setKeyNotice(null);
    try {
      const result = await session.sendTerminalText(taskId, terminalInput);
      if (!active.current) return;
      if (result.ok) {
        setTerminalInput("");
      } else {
        setKeyNotice(sendFailureNotice(result.reason));
      }
    } catch {
      if (active.current) setKeyNotice("Terminal input could not be confirmed.");
    } finally {
      keyInFlight.current = false;
      if (active.current) setKeyPending(false);
    }
  };
  useEffect(() => {
    let activeRefresh = true;
    let inFlight = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    setTerminalAvailable(false);
    setText("");
    setError(null);
    setKeyNotice(null);
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
          setTerminalAvailable(true);
        }
      } catch (cause) {
        if (activeRefresh) {
          setTerminalAvailable(false);
          setError(cause instanceof Error ? cause.message : "Terminal unavailable");
        }
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
            disabled={keyPending || connectionStatus !== "ready" || !terminalAvailable}
            onClick={() => void pressKey(key)}
          >
            {label}
          </button>
        ))}
      </div>
      <p className="dm-native-remote__eyebrow">Live owner terminal · startup controls</p>
      {keyNotice ? <p role="status">{keyNotice}</p> : null}
      <pre role={error ? "status" : undefined}>
        {text || error || "Waiting for terminal output…"}
      </pre>
      <form className="dm-native-remote__terminal-input" onSubmit={submitTerminalInput}>
        <input
          aria-label="Terminal input"
          autoCapitalize="none"
          autoComplete="off"
          disabled={keyPending || connectionStatus !== "ready" || !terminalAvailable}
          onChange={(event) => setTerminalInput(event.target.value)}
          placeholder="Type in the remote terminal…"
          spellCheck={false}
          value={terminalInput}
        />
        <button
          aria-label="Send terminal input"
          disabled={
            keyPending ||
            connectionStatus !== "ready" ||
            !terminalAvailable ||
            terminalInput.length === 0
          }
          type="submit"
        >
          <Send aria-hidden="true" size={17} />
        </button>
      </form>
    </section>
  );
}

function SemanticTimeline({
  answerableQuestionId,
  answering,
  facts,
  items,
  onAnswerQuestion,
  taskId,
}: {
  answerableQuestionId: string | null;
  answering: boolean;
  facts: readonly SemanticJournalFact[];
  items: ReturnType<typeof buildNativeTimeline>;
  onAnswerQuestion: (answer: string) => Promise<boolean>;
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
                  {item.id === answerableQuestionId ? (
                    <QuestionAnswer
                      answering={answering}
                      onAnswer={onAnswerQuestion}
                      options={item.options}
                    />
                  ) : item.options.length ? (
                    <ul>{item.options.map((option) => <li key={option}>{option}</li>)}</ul>
                  ) : <small>Answered</small>}
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

function QuestionAnswer({
  answering,
  onAnswer,
  options,
}: {
  answering: boolean;
  onAnswer: (answer: string) => Promise<boolean>;
  options: string[];
}) {
  const [answer, setAnswer] = useState("");
  const submit = async (value: string) => {
    if (await onAnswer(value)) setAnswer("");
  };
  return (
    <div className="dm-native-remote__question-controls">
      {options.length ? (
        <div className="dm-native-remote__question-options">
          {options.map((option) => (
            <button disabled={answering} key={option} onClick={() => void submit(option)} type="button">
              {option}
            </button>
          ))}
        </div>
      ) : null}
      <form onSubmit={(event) => { event.preventDefault(); void submit(answer); }}>
        <input
          aria-label="Answer question"
          disabled={answering}
          onChange={(event) => setAnswer(event.target.value)}
          placeholder="Type an answer"
          value={answer}
        />
        <button disabled={answering || !answer.trim()} type="submit">Answer</button>
      </form>
    </div>
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
  const status = visibleTaskStatus(task);
  const metadata = [hostBadge, status.label].filter(Boolean).join(" · ");
  return (
    <button className="dm-native-remote__task-row" onClick={onOpen} type="button">
      <span className="dm-native-remote__task-copy">
        <strong>{task.title ?? "Task"}</strong>
        <small className={`dm-native-remote__task-status dm-native-remote__task-status--${status.tone}`}>
          {metadata}
        </small>
      </span>
      <ChevronRight aria-hidden="true" size={18} />
    </button>
  );
}

function visibleTaskStatus(task: TaskMeta): { label: string; tone: string } {
  if (isClosingTask(task)) return { label: "Archiving…", tone: "working" };
  if (task.connectivity === "disconnected") return { label: "Offline", tone: "offline" };
  if (task.attention === "failed") return { label: "Failed", tone: "danger" };
  if (task.attention === "uncertain_outcome") return { label: "Check outcome", tone: "danger" };
  if (task.attention === "needs_approval") return { label: "Approval needed", tone: "attention" };
  if (task.attention === "needs_answer") return { label: "Reply needed", tone: "attention" };
  if (task.activity === "working" || task.activity === "active") {
    return { label: "Working", tone: "working" };
  }
  if (task.activity === "settling") return { label: "Finishing", tone: "working" };
  return { label: "Waiting", tone: "waiting" };
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

function sendFailureNotice(reason: NativeSendFailure): string {
  switch (reason) {
    case "no_agent":
      return "This task has no active agent session. Your draft is unchanged; restore or restart its provider from the desktop app.";
    case "blockers":
      return "Answer the pending question or approval before sending another message. Your draft is unchanged.";
    case "storage_failure":
      return "The message could not be saved to the durable outbox. Your draft is unchanged.";
    case "client_mismatch":
      return "This browser connection changed before the message was sent. Your draft is unchanged; reconnect before retrying.";
    case "transport_uncertain":
    case "reconciliation_required":
      return "Delivery is not confirmed yet. Your message is retained on this device; checking its original receipt before retrying.";
    case "invalid_lifecycle":
      return "Restore this task before sending. Your draft is unchanged.";
    case "not_ready":
      return "The host is not ready to accept this message. Your draft is unchanged.";
    case "rejected":
      return "The host rejected this message. Your draft is unchanged.";
  }
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
