import { ArrowLeft, Columns3, Text, WifiOff } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import type { AppRoute } from "../app/router";
import { routeForTaskId, stableSessionKeyForRoute } from "../app/router";
import {
  isLiveStatus,
  type SemanticEvent,
  type WebProjectCommand,
  type WebWorkspaceSnapshot,
} from "../api/types";
import type { WsStatus } from "../api/ws";
import { BrowserView } from "../browser/BrowserView";
import type { BrowserProjection } from "../browser/model";
import {
  clearOtherRuntimes,
  loadDraft,
  removeDraft,
  saveDraft,
} from "../drafts/draftStore";
import { useDensityPreference } from "../settings/densityPreference";
import {
  useReturnBehavior,
  useTerminalPreference,
} from "../settings/inputPreference";
import { GuestActionNotice } from "../connect/GuestActionNotice";
import {
  canPerform,
  canUseOwnerControls,
  deriveComposerMode,
  deriveConnectUiGate,
  resolveCapabilityGrant,
  type CapabilityGrant,
} from "../connect/permissions";
import {
  applyHostProjection,
  createConnectClientSession,
  observeAuthoritativeSender,
  ownerBadge,
  requiresManualRefresh,
  visibleController,
} from "../connect/session";
import { useStore } from "../store";
import { DEFAULT_TASK_RESOURCE } from "./taskId";
import { Composer } from "./Composer";
import { describeTask } from "./taskModel";
import { resolveNativeSessionView, resolveViewMode } from "./viewMode";
import { AiSessionView } from "./views/AiSessionView";
import { CommandSessionView } from "./views/CommandSessionView";
import { RawTerminalView } from "./views/RawTerminalView";
import { ServerSessionView } from "./views/ServerSessionView";

function commandForSession(
  workspace: WebWorkspaceSnapshot,
  commandId: string | null,
): WebProjectCommand | null {
  if (!commandId) return null;
  for (const project of workspace.projects) {
    for (const folder of project.folders) {
      const command = folder.commands.find(
        (candidate) => candidate.id === commandId,
      );
      if (command) return command;
    }
  }
  return null;
}

function TaskUnavailable({
  onNavigate,
}: {
  onNavigate(route: AppRoute): void;
}) {
  return (
    <section className="dm-screen">
      <header className="dm-compact-header">
        <button
          type="button"
          className="dm-nav-back"
          onClick={() => onNavigate({ name: "tasks" })}
        >
          <ArrowLeft size={21} aria-hidden="true" /> Tasks
        </button>
      </header>
      <div className="dm-screen-scroll">
        <div className="dm-native-empty">
          <h2>Task unavailable</h2>
          <p>The DevManager host no longer includes this task.</p>
        </div>
      </div>
    </section>
  );
}

export function TaskScreen({
  route,
  workspace,
  status,
  onNavigate,
  demoEvents,
  grant: grantProp,
}: {
  route: Extract<AppRoute, { name: "task" }>;
  workspace: WebWorkspaceSnapshot;
  status: WsStatus;
  onNavigate(route: AppRoute): void;
  demoEvents?: SemanticEvent[];
  grant?: CapabilityGrant | null;
}) {
  const stableSessionKey = stableSessionKeyForRoute(route);
  const summary = workspace.sessions.find(
    (candidate) => candidate.stableSessionKey === stableSessionKey,
  );
  const journal = useStore((state) =>
    stableSessionKey ? state.journals[stableSessionKey] : undefined,
  );
  const storedDraft = useStore((state) =>
    stableSessionKey ? state.drafts[stableSessionKey] : undefined,
  );
  const mutationPending = useStore((state) =>
    stableSessionKey
      ? Boolean(state.pendingMutations[stableSessionKey])
      : false,
  );
  const writerLease = useStore((state) => state.writerLease);
  const setDraft = useStore((state) => state.setDraft);
  const setComposerSafety = useStore((state) => state.setComposerSafety);
  const clearComposerSafety = useStore((state) => state.clearComposerSafety);
  const submitComposer = useStore((state) => state.submitComposer);
  const prepareComposer = useStore((state) => state.prepareComposer);
  const interruptSession = useStore((state) => state.interruptSession);
  const sendAction = useStore((state) => state.sendAction);
  const restartAiTab = useStore((state) => state.restartAiTab);
  const connectSsh = useStore((state) => state.connectSsh);
  const restartSsh = useStore((state) => state.restartSsh);
  const disconnectSsh = useStore((state) => state.disconnectSsh);
  const foregroundConnection = useStore((state) => state.foregroundConnection);
  const refreshActiveConnection = useStore(
    (state) => state.refreshActiveConnection,
  );
  const sendInput = useStore((state) => state.sendInput);
  const [density] = useDensityPreference();
  const [returnBehavior] = useReturnBehavior();
  const [terminalPreference] = useTerminalPreference();
  const resource = route.resource ?? DEFAULT_TASK_RESOURCE;
  const [terminalPinned, setTerminalPinned] = useState(
    terminalPreference === "raw" || resource === "terminal",
  );
  const [providerInteractionLabel, setProviderInteractionLabel] = useState<
    string | null
  >(null);
  const latestDraft = useRef("");
  const loadedDraftKey = useRef<string | null>(null);
  const connectSessionRef = useRef(
    createConnectClientSession(stableSessionKey ?? route.taskId),
  );

  const item = useMemo(
    () => (summary ? describeTask(workspace, summary) : null),
    [summary, workspace],
  );
  const events = demoEvents ?? journal?.events ?? [];
  const draft = storedDraft ?? "";
  latestDraft.current = draft;

  useEffect(() => {
    if (!stableSessionKey) return;
    const loadKey = `${workspace.runtimeInstanceId}:${stableSessionKey}`;
    if (loadedDraftKey.current === loadKey) return;
    loadedDraftKey.current = loadKey;
    clearOtherRuntimes(workspace.runtimeInstanceId);
    const persisted = loadDraft(workspace.runtimeInstanceId, stableSessionKey);
    if (
      persisted !== null &&
      useStore.getState().drafts[stableSessionKey] === undefined
    ) {
      setDraft(stableSessionKey, persisted);
    }
  }, [setDraft, stableSessionKey, workspace.runtimeInstanceId]);

  useEffect(() => {
    if (!stableSessionKey) return;
    return () => clearComposerSafety(stableSessionKey);
  }, [clearComposerSafety, stableSessionKey]);

  useEffect(() => {
    if (!stableSessionKey) return;
    const onPageHide = () =>
      saveDraft(
        workspace.runtimeInstanceId,
        stableSessionKey,
        latestDraft.current,
      );
    globalThis.addEventListener?.("pagehide", onPageHide);
    return () => globalThis.removeEventListener?.("pagehide", onPageHide);
  }, [stableSessionKey, workspace.runtimeInstanceId]);

  useEffect(() => {
    setTerminalPinned(terminalPreference === "raw" || resource === "terminal");
    setProviderInteractionLabel(null);
  }, [
    resource,
    stableSessionKey,
    terminalPreference,
    workspace.runtimeInstanceId,
  ]);

  if (!stableSessionKey || !summary || !item) {
    return <TaskUnavailable onNavigate={onNavigate} />;
  }

  const connected = status.kind === "open";
  const grant = resolveCapabilityGrant({
    statusKind: status.kind,
    taskId: stableSessionKey,
    grant: grantProp,
  });
  const composerMode = deriveComposerMode(grant);
  const sendGate = deriveConnectUiGate({
    grant,
    action: "sendPrompt",
    statusKind: status.kind,
  });
  const mutateAllowed = canPerform(grant, "mutateTask") && connected;
  const answerAllowed = canPerform(grant, "answerRequest") && connected;
  const sendAllowed = sendGate.kind === "allowed";
  const ownerControls = canUseOwnerControls(grant);
  if (connectSessionRef.current.taskId !== stableSessionKey) {
    connectSessionRef.current = createConnectClientSession(stableSessionKey);
  }
  applyHostProjection(connectSessionRef.current, {
    taskId: stableSessionKey,
    revision: workspace.revision,
    turnEpoch: Math.max(1, writerLease.generation),
    focusEpoch: 1,
  });
  observeAuthoritativeSender(
    connectSessionRef.current,
    writerLease.ownerClientInstanceId,
  );
  const controller = visibleController(connectSessionRef.current);
  const badge = ownerBadge(connectSessionRef.current);
  const refreshRequired = requiresManualRefresh(connectSessionRef.current);
  const live = isLiveStatus(summary.status);
  const ai = summary.kind === "claude" || summary.kind === "codex";
  const provider = ai ? (summary.kind as "claude" | "codex") : null;
  const nativeView = resolveNativeSessionView(
    summary.kind,
    summary.interactiveShell === true,
  );
  const resolvedViewMode = resolveViewMode({
    adapterHealth: summary.adapterHealth,
    ai,
    gridInteractionRequired: summary.rawRequired,
    pinned: terminalPinned || resource === "terminal",
  });
  const viewMode =
    resource === "terminal" || providerInteractionLabel
      ? "terminal"
      : resolvedViewMode;
  const commandId =
    summary.commandId ??
    (stableSessionKey.startsWith("server:")
      ? stableSessionKey.slice("server:".length)
      : null);
  const command = commandForSession(workspace, commandId);
  const port = command?.port
    ? (workspace.portStatuses.find(
        (candidate) => candidate.port === command.port,
      ) ?? null)
    : null;
  const tab = workspace.tabs.find(
    (candidate) =>
      candidate.id === summary.tabId ||
      `tab:${candidate.id}` === stableSessionKey,
  );
  const controlNote =
    connected && writerLease.ownerClientInstanceId && !writerLease.youAreOwner
      ? "Active on another device · tap here to continue"
      : null;
  const composerNotice = (
    <GuestActionNotice
      grant={grant}
      action="sendPrompt"
      statusKind={status.kind}
    />
  );
  const composer =
    composerMode === "hidden" ? (
      composerNotice
    ) : (
      <>
        {sendGate.kind === "denied" ? composerNotice : null}
        <Composer
          key={`${workspace.runtimeInstanceId}:${stableSessionKey}`}
          scopeKey={`${workspace.runtimeInstanceId}:${stableSessionKey}`}
          value={draft}
          disabled={!connected}
          editingDisabled={!live}
          pending={mutationPending}
          supportsAttachments={ai}
          provider={provider ?? undefined}
          catalogSessionKey={stableSessionKey}
          returnBehavior={returnBehavior}
          placeholder={
            ai
              ? `Message ${summary.kind === "claude" ? "Claude" : "Codex"}`
              : "Enter a command"
          }
          note={controlNote}
          thinking={ai && summary.aiActivity === "Thinking"}
          onStop={() => {
            if (!mutateAllowed) return;
            interruptSession(stableSessionKey);
          }}
          onFocus={prepareComposer}
          onSafetyStateChange={(safety) =>
            setComposerSafety(stableSessionKey, safety)
          }
          onChange={(value) => {
            setDraft(stableSessionKey, value);
            saveDraft(workspace.runtimeInstanceId, stableSessionKey, value);
          }}
          onSubmit={async (text, attachments) => {
            if (!sendAllowed) {
              throw new Error("This action is not permitted.");
            }
            await submitComposer(stableSessionKey, text, attachments);
            removeDraft(workspace.runtimeInstanceId, stableSessionKey);
          }}
          onProviderCommandSubmitted={(command) => {
            if (!provider || !sendAllowed) return;
            setProviderInteractionLabel(
              `${provider === "claude" ? "Claude" : "Codex"} · ${command.name}`,
            );
            setTerminalPinned(true);
            onNavigate(routeForTaskId(route.taskId, "terminal"));
          }}
        />
      </>
    );

  let content;
  if (resource === "browser") {
    const projection: BrowserProjection = {
      taskId: route.taskId,
      contextId: route.taskId,
      generation: 0,
      boundsEpoch: 0,
      focusEpoch: 0,
      frameId: 0,
      tabs: [],
      interactionMode: "observe",
    };
    content = <BrowserView projection={projection} />;
  } else if (viewMode === "terminal") {
    content = (
      <RawTerminalView
        sessionId={summary.sessionId}
        interactionLabel={providerInteractionLabel ?? undefined}
      />
    );
  } else if (nativeView === "ai") {
    content = (
      <AiSessionView
        events={events}
        density={density}
        adapterHealth={summary.adapterHealth}
        running={live}
        actionsDisabled={!mutateAllowed}
        questionChoicesDisabled={
          !answerAllowed ||
          !live ||
          mutationPending ||
          summary.attention !== "needsInput"
        }
        composer={composer}
        onRestart={() => {
          if (!mutateAllowed || !tab) return;
          void restartAiTab(tab.id);
        }}
        onQuestionChoice={(choice) => {
          if (!answerAllowed) return;
          const draftSnapshot = latestDraft.current;
          const submission = submitComposer(stableSessionKey, choice, []);
          // Restore without setDraft: setDraft cancels a pending mutation when
          // the restored text differs from the in-flight choice.
          useStore.setState((state) => ({
            drafts: { ...state.drafts, [stableSessionKey]: draftSnapshot },
          }));
          saveDraft(
            workspace.runtimeInstanceId,
            stableSessionKey,
            draftSnapshot,
          );
          void submission.catch(() => {
            // Store already records lastError for the rejected submission.
          });
        }}
      />
    );
  } else if (nativeView === "server") {
    content = (
      <ServerSessionView
        session={summary}
        command={command}
        port={port}
        events={events}
        density={density}
        actionsDisabled={!mutateAllowed}
        onStart={() => {
          if (!mutateAllowed || !commandId) return;
          sendAction({ type: "startServer", command_id: commandId });
        }}
        onStop={() => {
          if (!mutateAllowed || !commandId) return;
          sendAction({ type: "stopServer", command_id: commandId });
        }}
        onRestart={() => {
          if (!mutateAllowed || !commandId) return;
          sendAction({ type: "restartServer", command_id: commandId });
        }}
      />
    );
  } else {
    content = (
      <CommandSessionView
        events={events}
        density={density}
        connected={live}
        actionsDisabled={!mutateAllowed}
        composer={composer}
        onReconnect={
          !mutateAllowed
            ? undefined
            : summary.kind === "ssh" && tab?.connectionId
              ? () => connectSsh(tab.connectionId as string)
              : commandId
                ? () => sendAction({ type: "startServer", command_id: commandId })
                : undefined
        }
        onRestart={
          !mutateAllowed
            ? undefined
            : summary.kind === "ssh" && tab?.connectionId
              ? () => restartSsh(tab.connectionId as string)
              : summary.kind === "server" && commandId
                ? () =>
                    sendAction({ type: "restartServer", command_id: commandId })
                : undefined
        }
        onDisconnect={
          !mutateAllowed
            ? undefined
            : summary.kind === "ssh" && tab?.connectionId
              ? () => disconnectSsh(tab.connectionId as string)
              : summary.kind === "server" && commandId
                ? () => sendAction({ type: "stopServer", command_id: commandId })
                : undefined
        }
        disconnectLabel={summary.kind === "server" ? "Stop" : "Disconnect"}
      />
    );
  }

  return (
    <section
      className="dm-screen dm-session-detail-screen"
      aria-labelledby="task-title"
    >
      <header className="dm-session-header">
        <button
          type="button"
          className="dm-nav-back dm-session-back"
          onClick={() => onNavigate({ name: "tasks" })}
        >
          <ArrowLeft size={21} aria-hidden="true" /> Tasks
        </button>
        <div className="dm-session-title-block">
          <h1 id="task-title">{item.label}</h1>
          <p>
            {item.projectName} · {item.stateLabel}
          </p>
          {controller ? (
            <p data-testid="connect-visible-controller">
              Controller · {controller.clientId}
            </p>
          ) : null}
          {badge && ownerControls ? (
            <p data-testid="connect-owner-badge">Owner · rev {badge.revision}</p>
          ) : null}
        </div>
        <button
          type="button"
          className="dm-session-mode-button"
          aria-label={
            summary.rawRequired
              ? "Terminal grid required"
              : providerInteractionLabel
                ? "Return to native conversation"
                : viewMode === "terminal"
                  ? "Use native text view"
                  : "Use raw terminal"
          }
          disabled={summary.rawRequired}
          onClick={() => {
            if (resource === "browser") {
              onNavigate(routeForTaskId(route.taskId, "chat"));
              return;
            }
            if (viewMode === "terminal") {
              if (provider === "claude" && providerInteractionLabel) {
                // Claude keeps provider menus open after the web view returns
                // to native mode. Close the known interaction at that exact
                // boundary so the next native prompt starts in the composer.
                if (mutateAllowed) {
                  sendInput(summary.sessionId, "\u{1b}", "bytes");
                }
              }
              setProviderInteractionLabel(null);
              setTerminalPinned(false);
              onNavigate(routeForTaskId(route.taskId, "chat"));
              // Resume from the latest semantic cursor so output produced
              // while xterm was visible is reconciled before native render.
              foregroundConnection();
            } else {
              setProviderInteractionLabel(null);
              setTerminalPinned(true);
              onNavigate(routeForTaskId(route.taskId, "terminal"));
            }
          }}
        >
          {summary.rawRequired ? (
            <WifiOff size={19} />
          ) : viewMode === "terminal" ? (
            <Text size={19} />
          ) : (
            <Columns3 size={19} />
          )}
        </button>
      </header>
      {refreshRequired ? (
        <div data-testid="connect-manual-refresh" role="status">
          <span>Session is stale · refresh to resume safely.</span>
          <button
            type="button"
            onClick={() => refreshActiveConnection()}
          >
            Refresh session
          </button>
        </div>
      ) : null}
      {content}
    </section>
  );
}
