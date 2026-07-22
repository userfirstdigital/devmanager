import { describe, expect, it } from "vitest";

import type {
  WebSessionSummary,
  WebWorkspaceSnapshot,
} from "../api/types";
import { WEB_PROTOCOL_VERSION } from "../api/types";
import {
  countActionableAttention,
  describeSession,
  groupSessions,
} from "./sessionModel";

function session(
  stableSessionKey: string,
  overrides: Partial<WebSessionSummary> = {},
): WebSessionSummary {
  return {
    sessionId: `pty-${stableSessionKey}`,
    stableSessionKey,
    kind: stableSessionKey.startsWith("server:") ? "server" : "claude",
    status: "Running",
    projectId: "project-devmanager",
    commandId: stableSessionKey.startsWith("server:")
      ? stableSessionKey.slice("server:".length)
      : null,
    tabId: stableSessionKey.startsWith("tab:")
      ? stableSessionKey.slice("tab:".length)
      : null,
    dimensions: { cols: 80, rows: 24, cell_width: 10, cell_height: 20 },
    lastActivityEpochMs: 1_000,
    attention: "none",
    attentionCount: 0,
    adapterHealth: "healthy",
    rawRequired: false,
    oldestSequence: 1,
    latestSequence: 2,
    ...overrides,
  };
}

function workspace(
  sessions: WebSessionSummary[],
  options: {
    tabs?: WebWorkspaceSnapshot["tabs"];
    sshConnections?: WebWorkspaceSnapshot["sshConnections"];
  } = {},
): WebWorkspaceSnapshot {
  return {
    webProtocolVersion: WEB_PROTOCOL_VERSION,
    runtimeInstanceId: "runtime",
    revision: 1,
    serverId: "server",
    projects: [
      {
        id: "project-devmanager",
        name: "DevManager",
        color: "#5b5bd6",
        folders: [
          {
            id: "web-folder",
            name: "Web",
            commands: [
              {
                id: "web",
                label: "Web app",
                port: 5199,
                status: "Running",
              },
            ],
          },
        ],
      },
    ],
    sshConnections: options.sshConnections ?? [
      {
        id: "ssh-home",
        label: "Home lab",
        host: "lab.local",
        port: 22,
        username: "dev",
      },
    ],
    tabs: options.tabs ?? [
      {
        id: "claude-a",
        kind: "claude",
        projectId: "project-devmanager",
        commandId: null,
        sessionId: "pty-tab:claude-a",
        connectionId: null,
        label: "Native mobile UI",
      },
      {
        id: "claude-generic",
        kind: "claude",
        projectId: "project-devmanager",
        commandId: null,
        sessionId: "pty-tab:claude-generic",
        connectionId: null,
        label: "Claude 6",
      },
      {
        id: "ssh-home",
        kind: "ssh",
        projectId: "project-devmanager",
        commandId: null,
        sessionId: "pty-tab:ssh-home",
        connectionId: "ssh-home",
        label: "Home lab",
      },
    ],
    sessions,
    portStatuses: [],
    writerLease: {
      ownerClientInstanceId: null,
      generation: 0,
      expiresAtEpochMs: null,
      youAreOwner: false,
    },
  };
}

describe("session presentation", () => {
  it("prefers meaningful runtime titles over generic AI tab labels", () => {
    const withTitle = session("tab:claude-generic", {
      title: "Ship live-first sessions",
      tabId: "claude-generic",
    });
    expect(describeSession(workspace([withTitle]), withTitle)).toMatchObject({
      label: "Ship live-first sessions",
      projectName: "DevManager",
      kindLabel: "Claude",
    });

    const genericOnly = session("tab:claude-generic", {
      tabId: "claude-generic",
    });
    expect(describeSession(workspace([genericOnly]), genericOnly).label).toBe(
      "Claude session",
    );

    const namedTab = session("tab:claude-a");
    expect(describeSession(workspace([namedTab]), namedTab).label).toBe(
      "Native mobile UI",
    );
  });

  it("uses semantic taskTitle when runtime and tab titles are generic provider chrome", () => {
    const decorated = session("tab:claude-generic", {
      title: "✳ Claude Code",
      taskTitle: "Investigate househunter listing sync",
      tabId: "claude-generic",
    });
    expect(describeSession(workspace([decorated]), decorated).label).toBe(
      "Investigate househunter listing sync",
    );

    const bareClaudeCode = session("tab:claude-generic", {
      title: "Claude Code",
      taskTitle: "Ship live-first sessions",
      tabId: "claude-generic",
    });
    expect(describeSession(workspace([bareClaudeCode]), bareClaudeCode).label).toBe(
      "Ship live-first sessions",
    );

    const openaiCodex = session("tab:codex-generic", {
      kind: "codex",
      title: "OpenAI Codex",
      taskTitle: "Review auth middleware",
      tabId: "codex-generic",
    });
    expect(
      describeSession(
        workspace([openaiCodex], {
          tabs: [
            {
              id: "codex-generic",
              kind: "codex",
              projectId: "project-devmanager",
              commandId: null,
              sessionId: "pty-tab:codex-generic",
              connectionId: null,
              label: "Codex 2",
            },
          ],
        }),
        openaiCodex,
      ).label,
    ).toBe("Review auth middleware");

    const taskTitleWinsOverRuntime = session("tab:claude-generic", {
      title: "✳ Claude Code",
      taskTitle: "Investigate househunter listing sync",
      tabId: "claude-generic",
    });
    expect(
      describeSession(workspace([taskTitleWinsOverRuntime]), taskTitleWinsOverRuntime)
        .label,
    ).toBe("Investigate househunter listing sync");

    const taskTitleBeatsMeaningfulRuntime = session("tab:claude-generic", {
      title: "Live provider chrome title",
      taskTitle: "Ship live-first sessions",
      tabId: "claude-generic",
    });
    expect(
      describeSession(
        workspace([taskTitleBeatsMeaningfulRuntime]),
        taskTitleBeatsMeaningfulRuntime,
      ).label,
    ).toBe("Ship live-first sessions");

    const customTabFallback = session("tab:claude-a", {
      title: "✳ Claude Code",
      tabId: "claude-a",
    });
    expect(
      describeSession(workspace([customTabFallback]), customTabFallback).label,
    ).toBe("Native mobile UI");

    const realTaskMentionsClaude = session("tab:claude-generic", {
      title: "Ask Claude about the listing sync",
      tabId: "claude-generic",
    });
    expect(
      describeSession(workspace([realTaskMentionsClaude]), realTaskMentionsClaude)
        .label,
    ).toBe("Ask Claude about the listing sync");
  });

  it("keeps meaningful runtime titles when no taskTitle exists, and rejects provider chrome fallbacks", () => {
    const meaningfulRuntime = session("tab:claude-generic", {
      title: "Ship live-first sessions",
      tabId: "claude-generic",
    });
    expect(describeSession(workspace([meaningfulRuntime]), meaningfulRuntime).label).toBe(
      "Ship live-first sessions",
    );

    const spinnerGlyph = session("tab:claude-generic", {
      title: "✳",
      tabId: "claude-generic",
    });
    expect(describeSession(workspace([spinnerGlyph]), spinnerGlyph).label).toBe(
      "Claude session",
    );

    const shellPrompt = session("tab:claude-generic", {
      title: "user@host:~/devmanager$",
      tabId: "claude-generic",
    });
    expect(describeSession(workspace([shellPrompt]), shellPrompt).label).toBe(
      "Claude session",
    );

    const pathLike = session("tab:claude-generic", {
      title: "C:\\Windows\\system32\\cmd.exe",
      tabId: "claude-generic",
    });
    expect(describeSession(workspace([pathLike]), pathLike).label).toBe("Claude session");

    const mingwPrompt = session("tab:claude-generic", {
      title: "MINGW64:/c/Code/personal/househunter",
      tabId: "claude-generic",
    });
    expect(describeSession(workspace([mingwPrompt]), mingwPrompt).label).toBe(
      "Claude session",
    );

    const brailleSpinner = session("tab:claude-generic", {
      title: "⠐ Working",
      tabId: "claude-generic",
    });
    expect(describeSession(workspace([brailleSpinner]), brailleSpinner).label).toBe(
      "Claude session",
    );

    const spinnerDoesNotReplaceTask = session("tab:claude-generic", {
      title: "⠋ Thinking…",
      taskTitle: "Normalize session titles",
      tabId: "claude-generic",
    });
    expect(
      describeSession(workspace([spinnerDoesNotReplaceTask]), spinnerDoesNotReplaceTask)
        .label,
    ).toBe("Normalize session titles");
  });

  it("rejects bare, session-suffixed, and numbered generic AI labels for title and tab", () => {
    const forms = ["Claude", "claude session", "Claude 6", "CLAUDE 12"] as const;
    for (const form of forms) {
      const fromTitle = session("tab:claude-a", {
        title: form,
        tabId: "claude-a",
      });
      expect(describeSession(workspace([fromTitle]), fromTitle).label).toBe(
        "Native mobile UI",
      );

      const genericTab = session("tab:claude-generic", {
        title: form,
        tabId: "claude-generic",
      });
      expect(describeSession(workspace([genericTab]), genericTab).label).toBe(
        "Claude session",
      );
    }

    const codexForms = ["Codex", "codex session", "Codex 3"] as const;
    for (const form of codexForms) {
      const codex = session("tab:codex-generic", {
        kind: "codex",
        title: form,
        tabId: "codex-generic",
      });
      expect(
        describeSession(
          workspace([codex], {
            tabs: [
              {
                id: "codex-generic",
                kind: "codex",
                projectId: "project-devmanager",
                commandId: null,
                sessionId: "pty-tab:codex-generic",
                connectionId: null,
                label: form,
              },
            ],
          }),
          codex,
        ).label,
      ).toBe("Codex session");
    }
  });

  it("retains configured command and SSH labels", () => {
    const server = session("server:web", {
      title: "node server.js",
    });
    expect(describeSession(workspace([server]), server)).toMatchObject({
      label: "Web app",
      kindLabel: "Server",
    });

    const ssh = session("tab:ssh-home", {
      kind: "ssh",
      title: "ssh lab.local",
      tabId: "ssh-home",
    });
    expect(describeSession(workspace([ssh]), ssh)).toMatchObject({
      label: "Home lab",
      kindLabel: "SSH",
      stateLabel: "Connected",
    });
  });

  it("exposes Thinking, Ready, Needs input, terminal fallback, and live failure states", () => {
    const thinking = session("tab:claude-a", { aiActivity: "Thinking" });
    expect(describeSession(workspace([thinking]), thinking)).toMatchObject({
      stateLabel: "Thinking",
      statusTone: "active",
    });

    const ready = session("tab:claude-a", { attention: "unread", attentionCount: 2 });
    expect(describeSession(workspace([ready]), ready)).toMatchObject({
      stateLabel: "Ready",
      statusTone: "attention",
    });

    const readyBeatsThinking = session("tab:claude-a", {
      attention: "unread",
      attentionCount: 1,
      aiActivity: "Thinking",
    });
    expect(
      describeSession(workspace([readyBeatsThinking]), readyBeatsThinking),
    ).toMatchObject({
      stateLabel: "Ready",
      statusTone: "attention",
    });

    const needsInput = session("tab:claude-a", { attention: "needsInput" });
    expect(describeSession(workspace([needsInput]), needsInput)).toMatchObject({
      stateLabel: "Needs input",
      statusTone: "attention",
    });

    const endedNeedsInputStale = session("tab:claude-a", {
      status: "Failed",
      attention: "needsInput",
    });
    expect(
      describeSession(workspace([endedNeedsInputStale]), endedNeedsInputStale),
    ).toMatchObject({
      stateLabel: "Needs attention",
      statusTone: "danger",
    });

    const endedCrashedNeedsInput = session("tab:claude-a", {
      status: "Crashed",
      attention: "needsInput",
    });
    expect(
      describeSession(workspace([endedCrashedNeedsInput]), endedCrashedNeedsInput),
    ).toMatchObject({
      stateLabel: "Crashed",
      statusTone: "danger",
    });

    const terminalFallback = session("tab:claude-a", {
      adapterHealth: "degraded",
      rawRequired: true,
    });
    expect(
      describeSession(workspace([terminalFallback]), terminalFallback),
    ).toMatchObject({
      stateLabel: "Terminal fallback",
      statusTone: "attention",
    });

    const liveFailed = session("tab:claude-a", { attention: "failed" });
    expect(describeSession(workspace([liveFailed]), liveFailed)).toMatchObject({
      stateLabel: "Needs attention",
      statusTone: "danger",
    });
  });

  it("presents ended Stopped/Exited lifecycle before stale semantic attention", () => {
    const stoppedNeedsInput = session("tab:claude-a", {
      status: "Stopped",
      attention: "needsInput",
    });
    expect(
      describeSession(workspace([stoppedNeedsInput]), stoppedNeedsInput),
    ).toMatchObject({
      stateLabel: "Ready to reopen",
      statusTone: "neutral",
    });

    const exitedFailed = session("tab:claude-a", {
      status: "Exited",
      attention: "failed",
    });
    expect(describeSession(workspace([exitedFailed]), exitedFailed)).toMatchObject({
      stateLabel: "Ready to reopen",
      statusTone: "neutral",
    });

    const sshStoppedFailed = session("tab:ssh-home", {
      kind: "ssh",
      status: "Stopped",
      attention: "failed",
      tabId: "ssh-home",
    });
    expect(
      describeSession(workspace([sshStoppedFailed]), sshStoppedFailed),
    ).toMatchObject({
      stateLabel: "Disconnected",
      statusTone: "neutral",
    });

    const serverExitedNeedsInput = session("server:web", {
      status: "Exited",
      attention: "needsInput",
    });
    expect(
      describeSession(workspace([serverExitedNeedsInput]), serverExitedNeedsInput),
    ).toMatchObject({
      stateLabel: "Exited",
      statusTone: "neutral",
    });
  });

  it("keeps live needsInput and live-failed in Live now; ended failures are recent", () => {
    const needsInput = session("tab:needs", {
      attention: "needsInput",
      attentionCount: 2,
      lastActivityEpochMs: 4_000,
    });
    const liveFailed = session("tab:live-failed", {
      attention: "failed",
      status: "Running",
      lastActivityEpochMs: 5_000,
    });
    const ready = session("tab:ready", {
      attention: "unread",
      attentionCount: 1,
      lastActivityEpochMs: 4_200,
    });
    const starting = session("tab:starting", {
      status: "Starting",
      lastActivityEpochMs: 4_500,
    });
    const stopping = session("tab:stopping", {
      status: "Stopping",
      attention: "failed",
      lastActivityEpochMs: 3_500,
    });
    const endedFailed = session("tab:ended-failed", {
      attention: "failed",
      status: "Failed",
      lastActivityEpochMs: 6_000,
    });
    const crashed = session("tab:crashed", {
      attention: "failed",
      status: "Crashed",
      lastActivityEpochMs: 7_000,
    });
    const thinking = session("tab:thinking", {
      aiActivity: "Thinking",
      lastActivityEpochMs: 3_000,
    });
    const idleLive = session("tab:idle", {
      lastActivityEpochMs: 2_500,
    });
    const recent = session("server:web", {
      status: "Stopped",
      lastActivityEpochMs: 1_000,
    });

    const groups = groupSessions(
      workspace([
        recent,
        idleLive,
        thinking,
        ready,
        stopping,
        starting,
        needsInput,
        liveFailed,
        endedFailed,
        crashed,
      ]),
    );

    expect(groups.live.map((item) => item.stableSessionKey)).toEqual([
      "tab:needs",
      "tab:live-failed",
      "tab:stopping",
      "tab:ready",
      "tab:thinking",
      "tab:starting",
      "tab:idle",
    ]);
    expect(groups.recent.map((item) => item.stableSessionKey)).toEqual([
      "tab:crashed",
      "tab:ended-failed",
      "server:web",
    ]);
    expect([...groups.live, ...groups.recent]).toHaveLength(10);
    expect(groups.live[0]).toMatchObject({ projectName: "DevManager" });
  });

  it("uses explicit, non-secret fallbacks when configuration is missing", () => {
    const orphan = session("tab:orphan", {
      projectId: "gone",
      kind: "ssh",
    });
    expect(describeSession(workspace([orphan]), orphan)).toMatchObject({
      label: "SSH session",
      projectName: "Project unavailable",
      stateLabel: "Connected",
    });
  });

  it("counts live non-none attention including Ready and excludes ended stale attention", () => {
    const groups = workspace([
      session("tab:needs", { attention: "needsInput", status: "Running" }),
      session("tab:live-failed", { attention: "failed", status: "Running" }),
      session("tab:stale-failed", { attention: "failed", status: "Failed" }),
      session("tab:ready", { attention: "unread", status: "Running" }),
      session("tab:stale-ready", { attention: "unread", status: "Stopped" }),
    ]);
    expect(countActionableAttention(groups)).toBe(3);
    expect(countActionableAttention(null)).toBe(0);
  });
});
