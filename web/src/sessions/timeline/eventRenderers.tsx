import {
  ChevronDown,
  CircleAlert,
  CircleCheck,
  CircleEllipsis,
  Terminal,
} from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";

import type { SemanticEvent } from "../../api/types";
import { MarkdownMessage } from "./MarkdownMessage";
import type { ConversationItem } from "./timelineModel";

export type InterfaceDensity = "minimal" | "calm" | "full";

function activityDetail(event: SemanticEvent): ReactNode {
  switch (event.kind) {
    case "reasoning":
      return event.summary;
    case "tool":
      return event.summary || `${event.name} ${event.state}`;
    case "diff":
      return <pre>{event.unified_diff}</pre>;
    case "command":
      return <code>$ {event.text}</code>;
    default:
      return null;
  }
}

function activityTitle(event: SemanticEvent): string {
  switch (event.kind) {
    case "reasoning":
      return "Thinking";
    case "tool":
      return `${event.name} · ${event.state}`;
    case "diff":
      return "Code changes";
    case "command":
      return event.exit_code === null
        ? "Command running"
        : `Command exited ${event.exit_code}`;
    default:
      return "Activity";
  }
}

function ActivityGroup({
  item,
  density,
}: {
  item: Extract<ConversationItem, { kind: "activity" }>;
  density: InterfaceDensity;
}) {
  // Calm/minimal stay collapsed while work is active; failures open; full stays open.
  const preferOpen = density === "full" || item.state === "failure";
  const [expanded, setExpanded] = useState(preferOpen);
  const [manual, setManual] = useState(false);
  useEffect(() => {
    if (!manual && preferOpen) setExpanded(true);
    if (!manual && !preferOpen && item.state === "active") setExpanded(false);
  }, [item.state, manual, preferOpen]);
  const StatusIcon =
    item.state === "failure"
      ? CircleAlert
      : item.state === "success"
        ? CircleCheck
        : CircleEllipsis;

  return (
    <article
      className={`dm-activity-group${expanded ? " is-expanded" : ""}`}
      data-state={item.state}
      data-event-sequence={item.sequence}
    >
      <button
        type="button"
        className="dm-activity-toggle"
        aria-expanded={expanded}
        onClick={() => {
          setManual(true);
          setExpanded((current) => !current);
        }}
      >
        <StatusIcon size={16} aria-hidden="true" />
        <span>{item.summary}</span>
        <ChevronDown className="dm-event-chevron" size={15} aria-hidden="true" />
      </button>
      {expanded ? (
        <div className="dm-activity-rows">
          {item.events.map((event) => (
            <div className="dm-activity-row" key={`${event.kind}:${event.sequence}`}>
              <strong>{activityTitle(event)}</strong>
              <div className="dm-activity-detail">{activityDetail(event)}</div>
            </div>
          ))}
        </div>
      ) : null}
    </article>
  );
}

export function ConversationItemRenderer({
  item,
  density,
  onQuestionChoice,
  questionChoicesDisabled = false,
}: {
  item: ConversationItem;
  density: InterfaceDensity;
  onQuestionChoice?(choice: string): void;
  questionChoicesDisabled?: boolean;
}) {
  if (item.kind === "message") {
    if (item.role === "user") {
      return (
        <article
          className="dm-message dm-message-user"
          data-event-sequence={item.sequence}
        >
          <p>{item.text}</p>
        </article>
      );
    }
    return (
      <article
        className="dm-message dm-message-assistant"
        data-event-sequence={item.sequence}
        aria-busy={item.streaming || undefined}
      >
        <MarkdownMessage text={item.text} />
        {item.streaming ? (
          <span className="dm-streaming-dot" aria-label="Responding" />
        ) : null}
      </article>
    );
  }

  if (item.kind === "activity") {
    return <ActivityGroup item={item} density={density} />;
  }

  if (item.kind === "question") {
    return (
      <article className="dm-question-card" data-event-sequence={item.sequence}>
        <CircleAlert size={19} aria-hidden="true" />
        <div>
          <strong>Input needed</strong>
          <p>{item.event.prompt}</p>
          {item.event.choices.length > 0 ? (
            <ul className="dm-question-choices">
              {item.event.choices.map((choice) => (
                <li key={choice}>
                  <button
                    type="button"
                    className="dm-question-choice"
                    disabled={questionChoicesDisabled || !onQuestionChoice}
                    onClick={() => onQuestionChoice?.(choice)}
                  >
                    {choice}
                  </button>
                </li>
              ))}
            </ul>
          ) : null}
        </div>
      </article>
    );
  }

  if (item.kind === "error") {
    return (
      <article
        className="dm-error-card"
        data-event-sequence={item.sequence}
        role="alert"
      >
        <CircleAlert size={19} aria-hidden="true" />
        <div>
          <strong>Something needs attention</strong>
          <p>{item.event.message}</p>
        </div>
      </article>
    );
  }

  return (
    <article
      className="dm-fallback-output"
      data-stream={item.stream}
      data-event-sequence={item.sequence}
    >
      <span>
        <Terminal size={14} aria-hidden="true" /> Limited transcript detail
      </span>
      <pre>{item.text}</pre>
    </article>
  );
}
