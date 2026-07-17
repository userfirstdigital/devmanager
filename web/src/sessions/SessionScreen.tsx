import { ArrowLeft, Columns3, Text, WifiOff } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

import type { AppRoute } from "../app/router";
import { stableSessionKeyForRoute } from "../app/router";
import {
  isLiveStatus,
  type SemanticEvent,
  type WebProjectCommand,
  type WebWorkspaceSnapshot,
} from "../api/types";
import type { WsStatus } from "../api/ws";
import { clearOtherRuntimes, loadDraft, removeDraft, saveDraft } from "../drafts/draftStore";
import { useDensityPreference } from "../settings/densityPreference";
import {
  useReturnBehavior,
  useTerminalPreference,
} from "../settings/inputPreference";
import { useStore } from "../store";
import { Composer } from "./Composer";
import { describeSession } from "./sessionModel";
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
      const command = folder.commands.find((candidate) => candidate.id === commandId);
      if (command) return command;
    }
  }
  return null;
}

function SessionUnavailable({ onNavigate }: { onNavigate(route: AppRoute): void }) {
  return (
    <section className="dm-screen">
      <header className="dm-compact-header">
        <button type="button" className="dm-nav-back" onClick={() => onNavigate({ name: "sessions" })}>
          <ArrowLeft size={21} aria-hidden="true" /> Sessions
        </button>
      </header>
      <div className="dm-screen-scroll">
        <div className="dm-native-empty">
          <h2>Session unavailable</h2>
          <p>The DevManager host no longer includes this session.</p>
        </div>
      </div>
    </section>
  );
}

export function SessionScreen({
  route,
  workspace,
  status,
  onNavigate,
  demoEvents,
}: {
  route: Extract<AppRoute, { name: "session" }>;
  workspace: WebWorkspaceSnapshot;
  status: WsStatus;
  onNavigate(route: AppRoute): void;
  demoEvents?: SemanticEvent[];
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
    stableSessionKey ? Boolean(state.pendingMutations[stableSessionKey]) : false,
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
  const sendInput = useStore((state) => state.sendInput);
  const [density] = useDensityPreference();
  const [returnBehavior] = useReturnBehavior();
  const [terminalPreference] = useTerminalPreference();
  const [terminalPinned, setTerminalPinned] = useState(
    terminalPreference === "raw",
  );
  const [providerInteractionLabel, setProviderInteractionLabel] = useState<string | null>(null);
  const latestDraft = useRef("");
  const loadedDraftKey = useRef<string | null>(null);

  const item = useMemo(
    () => (summary ? describeSession(workspace, summary) : null),
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
    if (persisted !== null && useStore.getState().drafts[stableSessionKey] === undefined) {
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
      saveDraft(workspace.runtimeInstanceId, stableSessionKey, latestDraft.current);
    globalThis.addEventListener?.("pagehide", onPageHide);
    return () => globalThis.removeEventListener?.("pagehide", onPageHide);
  }, [stableSessionKey, workspace.runtimeInstanceId]);

  useEffect(() => {
    setTerminalPinned(terminalPreference === "raw");
    setProviderInteractionLabel(null);
  }, [stableSessionKey, terminalPreference, workspace.runtimeInstanceId]);

  if (!stableSessionKey || !summary || !item) {
    return <SessionUnavailable onNavigate={onNavigate} />;
  }

  const connected = status.kind === "open";
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
    pinned: terminalPinned,
  });
  const viewMode = providerInteractionLabel ? "terminal" : resolvedViewMode;
  const commandId = summary.commandId ??
    (stableSessionKey.startsWith("server:") ? stableSessionKey.slice("server:".length) : null);
  const command = commandForSession(workspace, commandId);
  const port = command?.port
    ? workspace.portStatuses.find((candidate) => candidate.port === command.port) ?? null
    : null;
  const tab = workspace.tabs.find(
    (candidate) => candidate.id === summary.tabId || `tab:${candidate.id}` === stableSessionKey,
  );
  const controlNote =
    connected && writerLease.ownerClientInstanceId && !writerLease.youAreOwner
      ? "Active on another device · tap here to continue"
      : null;
  const composer = (
    <Composer
      key={`${workspace.runtimeInstanceId}:${stableSessionKey}`}
      scopeKey={`${workspace.runtimeInstanceId}:${stableSessionKey}`}
      value={draft}
      disabled={!connected || !live}
      pending={mutationPending}
      supportsAttachments={ai}
      provider={provider ?? undefined}
      catalogSessionKey={stableSessionKey}
      returnBehavior={returnBehavior}
      placeholder={ai ? `Message ${summary.kind === "claude" ? "Claude" : "Codex"}` : "Enter a command"}
      note={controlNote}
      onFocus={prepareComposer}
      onSafetyStateChange={(safety) =>
        setComposerSafety(stableSessionKey, safety)
      }
      onChange={(value) => {
        setDraft(stableSessionKey, value);
        saveDraft(workspace.runtimeInstanceId, stableSessionKey, value);
      }}
      onSubmit={async (text, attachments) => {
        await submitComposer(stableSessionKey, text, attachments);
        removeDraft(workspace.runtimeInstanceId, stableSessionKey);
      }}
      onProviderCommandSubmitted={(command) => {
        if (!provider) return;
        setProviderInteractionLabel(
          `${provider === "claude" ? "Claude" : "Codex"} · ${command.name}`,
        );
        setTerminalPinned(true);
      }}
    />
  );

  let content;
  if (viewMode === "terminal") {
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
        actionsDisabled={!connected}
        composer={composer}
        onInterrupt={() => interruptSession(stableSessionKey)}
        onRestart={() => {
          if (tab) void restartAiTab(tab.id);
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
        actionsDisabled={!connected}
        onStart={() => commandId && sendAction({ type: "startServer", command_id: commandId })}
        onStop={() => commandId && sendAction({ type: "stopServer", command_id: commandId })}
        onRestart={() => commandId && sendAction({ type: "restartServer", command_id: commandId })}
      />
    );
  } else {
    content = (
      <CommandSessionView
        events={events}
        density={density}
        connected={live}
        actionsDisabled={!connected}
        composer={composer}
        onReconnect={
          summary.kind === "ssh" && tab?.connectionId
            ? () => connectSsh(tab.connectionId as string)
            : commandId
              ? () => sendAction({ type: "startServer", command_id: commandId })
              : undefined
        }
        onRestart={
          summary.kind === "ssh" && tab?.connectionId
            ? () => restartSsh(tab.connectionId as string)
            : summary.kind === "server" && commandId
              ? () => sendAction({ type: "restartServer", command_id: commandId })
              : undefined
        }
        onDisconnect={
          summary.kind === "ssh" && tab?.connectionId
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
    <section className="dm-screen dm-session-detail-screen" aria-labelledby="session-title">
      <header className="dm-session-header">
        <button type="button" className="dm-nav-back dm-session-back" onClick={() => onNavigate({ name: "sessions" })}>
          <ArrowLeft size={21} aria-hidden="true" /> Sessions
        </button>
        <div className="dm-session-title-block">
          <h1 id="session-title">{item.label}</h1>
          <p>{item.projectName} · {item.stateLabel}</p>
        </div>
        <button
          type="button"
          className="dm-session-mode-button"
          aria-label={summary.rawRequired ? "Terminal grid required" : providerInteractionLabel ? "Return to native conversation" : viewMode === "terminal" ? "Use native text view" : "Use raw terminal"}
          disabled={summary.rawRequired}
          onClick={() => {
            if (viewMode === "terminal") {
              if (provider === "claude" && providerInteractionLabel) {
                // Claude keeps provider menus open after the web view returns
                // to native mode. Close the known interaction at that exact
                // boundary so the next native prompt starts in the composer.
                sendInput(summary.sessionId, "\u{1b}", "bytes");
              }
              setProviderInteractionLabel(null);
              setTerminalPinned(false);
              // Resume from the latest semantic cursor so output produced
              // while xterm was visible is reconciled before native render.
              foregroundConnection();
            } else {
              setProviderInteractionLabel(null);
              setTerminalPinned(true);
            }
          }}
        >
          {summary.rawRequired ? <WifiOff size={19} /> : viewMode === "terminal" ? <Text size={19} /> : <Columns3 size={19} />}
        </button>
      </header>
      {content}
    </section>
  );
}
