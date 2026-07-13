import { ChevronDown, CircleAlert, CircleCheck, CircleEllipsis, Terminal } from "lucide-react";
import { useState, type ReactNode } from "react";

import type { SemanticEvent } from "../../api/types";

export type InterfaceDensity = "minimal" | "calm" | "full";

function isActionable(event: SemanticEvent): boolean {
  return (
    event.kind === "question" ||
    event.kind === "error" ||
    (event.kind === "tool" && event.state === "failed")
  );
}

export function visibleEventsForDensity(
  events: SemanticEvent[],
  density: InterfaceDensity,
): SemanticEvent[] {
  if (density !== "minimal") return events;
  return events.filter(
    (event) =>
      event.kind === "userMessage" ||
      event.kind === "assistantMessage" ||
      isActionable(event),
  );
}

function CompactCard({
  title,
  detail,
  state,
  defaultExpanded,
  forceExpanded,
  mono = false,
}: {
  title: string;
  detail: ReactNode;
  state?: "active" | "success" | "failure";
  defaultExpanded: boolean;
  forceExpanded: boolean;
  mono?: boolean;
}) {
  const [expanded, setExpanded] = useState(defaultExpanded || forceExpanded);
  const shown = expanded;
  const StatusIcon =
    state === "failure" ? CircleAlert : state === "success" ? CircleCheck : CircleEllipsis;
  return (
    <article className={`dm-event-card${shown ? " is-expanded" : ""}`} data-state={state}>
      <button
        type="button"
        className="dm-event-card-toggle"
        aria-expanded={shown}
        onClick={() => setExpanded((current) => !current)}
      >
        <StatusIcon size={17} aria-hidden="true" />
        <span>{title}</span>
        <ChevronDown className="dm-event-chevron" size={16} aria-hidden="true" />
      </button>
      {shown && <div className={mono ? "dm-event-detail dm-mono" : "dm-event-detail"}>{detail}</div>}
    </article>
  );
}

function cardForEvent(event: SemanticEvent, density: InterfaceDensity): ReactNode {
  const full = density === "full";
  switch (event.kind) {
    case "reasoning":
      return <CompactCard title="Thinking" detail={event.summary} defaultExpanded={false} forceExpanded={full} />;
    case "tool": {
      const failed = event.state === "failed";
      return (
        <CompactCard
          title={`${event.name} · ${event.state}`}
          detail={event.summary || "No additional detail"}
          state={failed ? "failure" : event.state === "completed" ? "success" : "active"}
          defaultExpanded={failed}
          forceExpanded={full}
        />
      );
    }
    case "diff":
      return <CompactCard title="Code changes" detail={<pre>{event.unified_diff}</pre>} defaultExpanded={false} forceExpanded={full} mono />;
    case "command":
      return (
        <CompactCard
          title={event.exit_code === null ? "Command running" : `Command exited ${event.exit_code}`}
          detail={<code>$ {event.text}</code>}
          state={event.exit_code === null ? "active" : event.exit_code === 0 ? "success" : "failure"}
          defaultExpanded={event.exit_code !== 0 && event.exit_code !== null}
          forceExpanded={full}
          mono
        />
      );
    case "output":
      return <CompactCard title={event.stream === "stderr" ? "Error output" : "Output"} detail={<pre>{event.text}</pre>} state={event.stream === "stderr" ? "failure" : undefined} defaultExpanded={event.stream === "stderr"} forceExpanded={full} mono />;
    case "status":
      return <CompactCard title={event.state} detail={event.detail ?? "Session status updated"} defaultExpanded={false} forceExpanded={full} />;
    case "terminalMode":
      return <CompactCard title={event.raw_required ? "Terminal view needed" : "Native view restored"} detail={event.raw_required ? "This interaction needs a terminal grid." : "This session can be read as native text again."} defaultExpanded={false} forceExpanded={full} />;
    default:
      return null;
  }
}

export function EventRenderer({
  event,
  density,
}: {
  event: SemanticEvent;
  density: InterfaceDensity;
}) {
  if (event.kind === "userMessage") {
    return <article className="dm-message dm-message-user"><p>{event.text}</p></article>;
  }
  if (event.kind === "assistantMessage") {
    return (
      <article className="dm-message dm-message-assistant" aria-busy={event.streaming || undefined}>
        <p>{event.text}</p>
        {event.streaming && <span className="dm-streaming-dot" aria-label="Responding" />}
      </article>
    );
  }
  if (event.kind === "question") {
    return (
      <article className="dm-question-card">
        <CircleAlert size={19} aria-hidden="true" />
        <div>
          <strong>Input needed</strong>
          <p>{event.prompt}</p>
          {event.choices.length > 0 && (
            <ul>{event.choices.map((choice) => <li key={choice}>{choice}</li>)}</ul>
          )}
        </div>
      </article>
    );
  }
  if (event.kind === "error") {
    return (
      <article className="dm-error-card" role="alert">
        <CircleAlert size={19} aria-hidden="true" />
        <div><strong>Something needs attention</strong><p>{event.message}</p></div>
      </article>
    );
  }

  const card = cardForEvent(event, density);
  return card ? <>{card}</> : (
    <article className="dm-event-card is-expanded">
      <div className="dm-event-card-toggle"><Terminal size={17} aria-hidden="true" /><span>Session activity</span></div>
    </article>
  );
}
