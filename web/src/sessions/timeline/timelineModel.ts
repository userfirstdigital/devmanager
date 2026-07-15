import type {
  SemanticEvent,
  SemanticStream,
} from "../../api/types";
import type { InterfaceDensity } from "./eventRenderers";

type UserMessageEvent = Extract<SemanticEvent, { kind: "userMessage" }>;
type AssistantMessageEvent = Extract<SemanticEvent, { kind: "assistantMessage" }>;
type QuestionEvent = Extract<SemanticEvent, { kind: "question" }>;
type ErrorEvent = Extract<SemanticEvent, { kind: "error" }>;
type ActivityEvent = Extract<
  SemanticEvent,
  { kind: "reasoning" | "tool" | "diff" | "command" }
>;

export type ConversationItem =
  | {
      kind: "message";
      key: string;
      sequence: number;
      role: "user" | "assistant";
      text: string;
      streaming: boolean;
    }
  | {
      kind: "activity";
      key: string;
      sequence: number;
      events: ActivityEvent[];
      count: number;
      state: "active" | "success" | "failure";
      summary: string;
    }
  | {
      kind: "question";
      key: string;
      sequence: number;
      event: QuestionEvent;
    }
  | {
      kind: "error";
      key: string;
      sequence: number;
      event: ErrorEvent;
    }
  | {
      kind: "fallbackOutput";
      key: string;
      sequence: number;
      text: string;
      stream: SemanticStream;
    };

const MAX_FALLBACK_OUTPUT_CHARS = 12_000;

function activityIdentity(event: ActivityEvent): string {
  switch (event.kind) {
    case "reasoning":
      return `reasoning:${event.item_id}`;
    case "tool":
      return `tool:${event.tool_id}`;
    case "diff":
      return `diff:${event.item_id}`;
    case "command":
      return `command:${event.command_id}`;
  }
}

function activityLabel(event: ActivityEvent): string {
  switch (event.kind) {
    case "reasoning":
      return "Thinking";
    case "tool":
      return event.name;
    case "diff":
      return "Code changes";
    case "command":
      return "Command";
  }
}

function activityState(events: ActivityEvent[]): "active" | "success" | "failure" {
  if (
    events.some(
      (event) =>
        (event.kind === "tool" && event.state === "failed") ||
        (event.kind === "command" && event.exit_code !== null && event.exit_code !== 0),
    )
  ) {
    return "failure";
  }
  if (
    events.some(
      (event) =>
        (event.kind === "tool" && (event.state === "pending" || event.state === "running")) ||
        (event.kind === "command" && event.exit_code === null),
    )
  ) {
    return "active";
  }
  return "success";
}

function activitySummary(events: ActivityEvent[]): string {
  const labels = [...new Set(events.map(activityLabel))];
  return `${events.length} action${events.length === 1 ? "" : "s"} · ${labels.join(" · ")}`;
}

function boundFallbackOutput(text: string): string {
  if (text.length <= MAX_FALLBACK_OUTPUT_CHARS) return text;
  return `… earlier terminal text omitted …\n${text.slice(-MAX_FALLBACK_OUTPUT_CHARS + 36)}`;
}

export function buildConversationItems(
  events: SemanticEvent[],
  density: InterfaceDensity,
  includeFallbackOutput = true,
): ConversationItem[] {
  const items: ConversationItem[] = [];
  const assistantIndexes = new Map<string, number>();
  let pendingActivity: ActivityEvent[] = [];

  const flushActivity = () => {
    if (!pendingActivity.length) return;
    const state = activityState(pendingActivity);
    if (density !== "minimal" || state !== "success") {
      const first = pendingActivity[0];
      items.push({
        kind: "activity",
        key: `activity:${first.sequence}`,
        sequence: first.sequence,
        events: pendingActivity,
        count: pendingActivity.length,
        state,
        summary: activitySummary(pendingActivity),
      });
    }
    pendingActivity = [];
  };

  for (const event of events) {
    if (
      event.kind === "reasoning" ||
      event.kind === "tool" ||
      event.kind === "diff" ||
      event.kind === "command"
    ) {
      const identity = activityIdentity(event);
      const existing = pendingActivity.findIndex(
        (candidate) => activityIdentity(candidate) === identity,
      );
      if (existing >= 0) pendingActivity[existing] = event;
      else pendingActivity.push(event);
      continue;
    }

    if (event.kind === "status" || event.kind === "terminalMode") continue;
    flushActivity();

    if (event.kind === "userMessage") {
      const message = event as UserMessageEvent;
      items.push({
        kind: "message",
        key: `user:${message.sequence}`,
        sequence: message.sequence,
        role: "user",
        text: message.text,
        streaming: false,
      });
      continue;
    }

    if (event.kind === "assistantMessage") {
      const message = event as AssistantMessageEvent;
      const item: ConversationItem = {
        kind: "message",
        key: `assistant:${message.message_id}`,
        sequence: message.sequence,
        role: "assistant",
        text: message.text,
        streaming: message.streaming,
      };
      const existing = assistantIndexes.get(message.message_id);
      if (existing === undefined) {
        assistantIndexes.set(message.message_id, items.length);
        items.push(item);
      } else {
        items[existing] = item;
      }
      continue;
    }

    if (event.kind === "question") {
      items.push({
        kind: "question",
        key: `question:${event.question_id}`,
        sequence: event.sequence,
        event,
      });
      continue;
    }

    if (event.kind === "error") {
      items.push({
        kind: "error",
        key: `error:${event.sequence}`,
        sequence: event.sequence,
        event,
      });
      continue;
    }

    if (event.kind === "output" && density !== "minimal" && includeFallbackOutput) {
      const previous = items[items.length - 1];
      if (previous?.kind === "fallbackOutput" && previous.stream === event.stream) {
        previous.text = boundFallbackOutput(`${previous.text}${event.text}`);
        previous.sequence = event.sequence;
      } else {
        items.push({
          kind: "fallbackOutput",
          key: `fallback:${event.sequence}`,
          sequence: event.sequence,
          text: boundFallbackOutput(event.text),
          stream: event.stream,
        });
      }
    }
  }

  flushActivity();
  return items;
}
