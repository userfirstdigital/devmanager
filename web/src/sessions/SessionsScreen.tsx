import {
  Bot,
  ChevronRight,
  Clock3,
  Radio,
  Server,
  TerminalSquare,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import type { WebWorkspaceSnapshot } from "../api/types";
import type { AppRoute } from "../app/router";
import {
  formatRelativeActivity,
  groupSessions,
  type SessionListItem,
} from "./sessionModel";

interface SessionsScreenProps {
  workspace: WebWorkspaceSnapshot;
  onNavigate(route: AppRoute): void;
}

function SessionIcon({ item }: { item: SessionListItem }) {
  if (item.session.kind === "server") return <Server size={18} />;
  if (item.session.kind === "claude" || item.session.kind === "codex") {
    return <Bot size={18} />;
  }
  return <TerminalSquare size={18} />;
}

function SessionRow({
  item,
  now,
  onNavigate,
}: {
  item: SessionListItem;
  now: number;
  onNavigate(route: AppRoute): void;
}) {
  const activity = formatRelativeActivity(item.lastActivityEpochMs, now);
  const accessibleName = [
    item.label,
    item.projectName,
    item.kindLabel,
    item.stateLabel,
    activity,
  ].join(", ");
  const showUnreadBadge = item.attention === "unread" && item.attentionCount > 0;

  return (
    <button
      type="button"
      className="dm-session-row"
      aria-label={accessibleName}
      onClick={() => onNavigate(item.route)}
    >
      <span className="dm-session-kind-icon" data-tone={item.statusTone} aria-hidden="true">
        <SessionIcon item={item} />
      </span>
      <span className="dm-session-copy">
        <span className="dm-session-primary">
          <strong>{item.label}</strong>
          <span className="dm-session-time">{activity}</span>
        </span>
        <span className="dm-session-secondary">
          {item.projectColor ? (
            <i style={{ backgroundColor: item.projectColor }} aria-hidden="true" />
          ) : null}
          <span>{item.projectName}</span>
          <span aria-hidden="true">·</span>
          <span>{item.kindLabel}</span>
          <span aria-hidden="true">·</span>
          <span className="dm-session-state" data-tone={item.statusTone}>
            {item.stateLabel}
          </span>
        </span>
      </span>
      {showUnreadBadge ? (
        <span className="dm-attention-count" aria-label={`${item.attentionCount} unread updates`}>
          {Math.min(item.attentionCount, 99)}
        </span>
      ) : null}
      <ChevronRight className="dm-row-chevron" size={18} aria-hidden="true" />
    </button>
  );
}

export function SessionsScreen({ workspace, onNavigate }: SessionsScreenProps) {
  const [now, setNow] = useState(() => Date.now());
  const groups = useMemo(() => groupSessions(workspace), [workspace]);

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(timer);
  }, []);

  const sections = [
    {
      id: "live",
      title: "Live now",
      icon: <Radio size={15} aria-hidden="true" />,
      items: groups.live,
    },
    {
      id: "recent",
      title: "Recent",
      icon: <Clock3 size={15} aria-hidden="true" />,
      items: groups.recent,
    },
  ];
  const hasSessions = sections.some((section) => section.items.length > 0);

  return (
    <section className="dm-screen" aria-labelledby="sessions-title">
      <header className="dm-large-title-header">
        <div>
          <p className="dm-eyebrow">Your workspace</p>
          <h1 id="sessions-title">Sessions</h1>
        </div>
      </header>
      <div className="dm-screen-scroll">
        {!hasSessions ? (
          <div className="dm-native-empty">
            <span className="dm-native-empty-icon" aria-hidden="true">
              <TerminalSquare size={28} />
            </span>
            <h2>No sessions yet</h2>
            <p>Start Claude, Codex, or a server from Projects. New activity appears here automatically.</p>
            <button type="button" className="dm-primary-button" onClick={() => onNavigate({ name: "projects" })}>
              Open Projects
            </button>
          </div>
        ) : null}
        {sections.map((section, index) =>
          section.items.length ? (
            <section
              className={`dm-list-section${index === 0 ? " dm-list-section-first" : ""}`}
              key={section.id}
            >
              <h2>
                {section.icon}
                {section.title}
              </h2>
              <div className="dm-grouped-list">
                {section.items.map((item) => (
                  <SessionRow
                    key={item.stableSessionKey}
                    item={item}
                    now={now}
                    onNavigate={onNavigate}
                  />
                ))}
              </div>
            </section>
          ) : null,
        )}
      </div>
    </section>
  );
}
