import type { SemanticJournalFact } from "./nativeProtocol";

export type NativeTimelineItem =
  | {
      kind: "user";
      id: string;
      sequence: number;
      text: string;
    }
  | {
      kind: "assistant";
      id: string;
      sequence: number;
      messages: Array<{ id: string; text: string }>;
      reasoning: string[];
    }
  | {
      kind: "activity";
      id: string;
      sequence: number;
      title: string;
      details: string[];
    }
  | {
      kind: "question";
      id: string;
      sequence: number;
      prompt: string;
      options: string[];
    }
  | {
      kind: "error";
      id: string;
      sequence: number;
      message: string;
    };

/**
 * Converts the bounded canonical semantic journal into presentation items.
 * Adjacent assistant material shares one visual group so ordinary provider
 * output does not become a bubble per fact, while each canonical fact retains
 * its own text and identity.
 */
export function buildNativeTimeline(
  facts: readonly SemanticJournalFact[],
): NativeTimelineItem[] {
  // A semantic upsert keeps its stable fact id while receiving a newer durable
  // sequence. Keep the newest representation; never concatenate old partial
  // text with the replacement snapshot.
  const latestByFactId = new Map<string, SemanticJournalFact>();
  for (const fact of facts) {
    const prior = latestByFactId.get(fact.id);
    if (!prior || fact.sequence >= prior.sequence) {
      latestByFactId.set(fact.id, fact);
    }
  }
  const ordered = [...latestByFactId.values()].sort(
    (left, right) => left.sequence - right.sequence || left.id.localeCompare(right.id),
  );
  const timeline: NativeTimelineItem[] = [];
  const toolNames = new Map<string, string>();

  let assistant: Extract<NativeTimelineItem, { kind: "assistant" }> | null = null;
  const flushAssistant = () => {
    if (assistant) {
      timeline.push(assistant);
      assistant = null;
    }
  };

  const appendActivity = (sequence: number, factId: string, title: string, detail: string) => {
    const previous = timeline[timeline.length - 1];
    if (previous?.kind === "activity" && previous.title === title) {
      previous.details.push(detail);
      return;
    }
    timeline.push({
      kind: "activity",
      id: `activity-${factId}`,
      sequence,
      title,
      details: [detail],
    });
  };

  for (const fact of ordered) {
    const payload = fact.payload;
    if (fact.redacted) {
      flushAssistant();
      appendActivity(
        fact.sequence,
        fact.id,
        "Redacted activity",
        "This host did not share this detail.",
      );
      continue;
    }

    if (payload.kind === "assistant_text" || payload.kind === "reasoning_summary") {
      if (!assistant) {
        assistant = {
          kind: "assistant",
          id: `assistant-${fact.id}`,
          sequence: fact.sequence,
          messages: [],
          reasoning: [],
        };
      }
      if (payload.kind === "assistant_text") {
        assistant.messages.push({ id: fact.id, text: payload.text });
      } else if (payload.text.trim()) {
        assistant.reasoning.push(payload.text);
      }
      continue;
    }

    flushAssistant();
    switch (payload.kind) {
      case "user_message":
        timeline.push({
          kind: "user",
          id: `user-${fact.id}`,
          sequence: fact.sequence,
          text: payload.text,
        });
        break;
      case "tool_call":
        toolNames.set(payload.call_id, payload.tool_name);
        appendActivity(fact.sequence, fact.id, "Tool activity", payload.tool_name);
        break;
      case "tool_result":
        appendActivity(
          fact.sequence,
          fact.id,
          "Tool activity",
          `${toolNames.get(payload.call_id) ?? "Tool"} ${payload.status}`,
        );
        break;
      case "question":
        timeline.push({
          kind: "question",
          id: `question-${fact.id}`,
          sequence: fact.sequence,
          prompt: payload.prompt,
          options: payload.options,
        });
        break;
      case "error":
        timeline.push({
          kind: "error",
          id: `error-${fact.id}`,
          sequence: fact.sequence,
          message: payload.message,
        });
        break;
      default:
        appendActivity(fact.sequence, fact.id, activityTitle(payload.kind), activityDetail(fact));
        break;
    }
  }

  flushAssistant();
  return timeline;
}

function activityTitle(kind: SemanticJournalFact["payload"]["kind"]): string {
  switch (kind) {
    case "approval_request":
    case "approval_result":
      return "Approval";
    case "plan_step":
      return "Plan";
    case "usage_observation":
      return "Usage";
    case "turn_state":
    case "session_state":
      return "Session status";
    case "artifact_reference":
      return "Artifact";
    case "unknown":
      return "Provider activity";
    default:
      return "Activity";
  }
}

function activityDetail(fact: SemanticJournalFact): string {
  const payload = fact.payload;
  switch (payload.kind) {
    case "approval_request":
      return payload.summary;
    case "approval_result":
      return `Approval ${payload.decision}`;
    case "plan_step":
      return `${payload.title}: ${payload.status}`;
    case "usage_observation":
      return payload.remaining_percent === null
        ? "Usage was updated."
        : `${payload.remaining_percent}% remaining`;
    case "turn_state":
    case "session_state":
      return payload.state;
    case "artifact_reference":
      return payload.label;
    case "unknown":
      return `Unsupported ${payload.provider} event (${payload.source_type}).`;
    default:
      return "Activity updated.";
  }
}
