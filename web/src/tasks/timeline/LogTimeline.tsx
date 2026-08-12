import { ArrowDown, CircleAlert, Terminal } from "lucide-react";
import { useLayoutEffect, useMemo, useRef, useState } from "react";

import type { SemanticEvent, SemanticStream } from "../../api/types";

export type LogRow =
  | {
      kind: "output";
      key: string;
      sequence: number;
      stream: SemanticStream;
      text: string;
    }
  | {
      kind: "command";
      key: string;
      sequence: number;
      text: string;
      exitCode: number | null;
    }
  | {
      kind: "runBoundary";
      key: string;
      sequence: number;
      label: string;
    }
  | {
      kind: "error";
      key: string;
      sequence: number;
      message: string;
    }
  | {
      kind: "question";
      key: string;
      sequence: number;
      prompt: string;
    };

const FOLLOW_DISTANCE_PX = 96;

export function buildLogRows(events: SemanticEvent[]): LogRow[] {
  const rows: LogRow[] = [];
  let hasRunContent = false;

  for (const event of events) {
    if (event.kind === "output") {
      const previous = rows[rows.length - 1];
      if (previous?.kind === "output" && previous.stream === event.stream) {
        previous.text += event.text;
        previous.sequence = event.sequence;
      } else {
        rows.push({
          kind: "output",
          key: `output:${event.sequence}`,
          sequence: event.sequence,
          stream: event.stream,
          text: event.text,
        });
      }
      hasRunContent = true;
      continue;
    }

    if (event.kind === "command") {
      rows.push({
        kind: "command",
        key: `command:${event.command_id}:${event.sequence}`,
        sequence: event.sequence,
        text: event.text,
        exitCode: event.exit_code,
      });
      hasRunContent = true;
      continue;
    }

    if (event.kind === "status") {
      const state = event.state.toLowerCase();
      if ((state === "starting" || state === "running") && hasRunContent) {
        const previous = rows[rows.length - 1];
        if (previous?.kind !== "runBoundary") {
          rows.push({
            kind: "runBoundary",
            key: `run:${event.sequence}`,
            sequence: event.sequence,
            label: "New run",
          });
        }
        hasRunContent = false;
      } else if (["crashed", "exited", "failed"].includes(state)) {
        rows.push({
          kind: "runBoundary",
          key: `status:${event.sequence}`,
          sequence: event.sequence,
          label: event.detail || event.state,
        });
      }
      continue;
    }

    if (event.kind === "error") {
      rows.push({
        kind: "error",
        key: `error:${event.sequence}`,
        sequence: event.sequence,
        message: event.message,
      });
      continue;
    }

    if (event.kind === "question") {
      rows.push({
        kind: "question",
        key: `question:${event.question_id}`,
        sequence: event.sequence,
        prompt: event.prompt,
      });
    }
  }

  return rows;
}

function LogRowView({ row }: { row: LogRow }) {
  if (row.kind === "output") {
    return (
      <pre
        className="dm-log-output"
        data-stream={row.stream}
        data-event-sequence={row.sequence}
      >
        {row.text}
      </pre>
    );
  }
  if (row.kind === "command") {
    return (
      <div className="dm-log-command" data-event-sequence={row.sequence}>
        <Terminal size={15} aria-hidden="true" />
        <code>{row.text}</code>
        <span data-failed={row.exitCode !== null && row.exitCode !== 0 ? true : undefined}>
          {row.exitCode === null ? "running" : `exit ${row.exitCode}`}
        </span>
      </div>
    );
  }
  if (row.kind === "runBoundary") {
    return (
      <div className="dm-log-run-boundary" data-event-sequence={row.sequence}>
        <span>{row.label}</span>
      </div>
    );
  }
  return (
    <div
      className={row.kind === "error" ? "dm-log-alert is-error" : "dm-log-alert"}
      data-event-sequence={row.sequence}
      role={row.kind === "error" ? "alert" : "status"}
    >
      <CircleAlert size={16} aria-hidden="true" />
      <span>{row.kind === "error" ? row.message : row.prompt}</span>
    </div>
  );
}

export function LogTimeline({
  events,
  emptyTitle,
  emptyDetail,
}: {
  events: SemanticEvent[];
  emptyTitle: string;
  emptyDetail: string;
}) {
  const rows = useMemo(() => buildLogRows(events), [events]);
  const scrollRef = useRef<HTMLDivElement>(null);
  const followRef = useRef(true);
  const previousSequenceRef = useRef(0);
  const [unseenOutput, setUnseenOutput] = useState(false);

  const scrollToBottom = () => {
    const element = scrollRef.current;
    if (element) element.scrollTop = element.scrollHeight;
    followRef.current = true;
    setUnseenOutput(false);
  };

  useLayoutEffect(() => {
    const latestSequence = rows[rows.length - 1]?.sequence ?? 0;
    const grew = latestSequence > previousSequenceRef.current;
    previousSequenceRef.current = latestSequence;
    if (!grew) return;
    if (followRef.current) scrollToBottom();
    else setUnseenOutput(true);
  }, [rows]);

  return (
    <div className="dm-log-shell">
      <div
        ref={scrollRef}
        className="dm-log-scroll"
        role="log"
        aria-live="polite"
        onScroll={(event) => {
          const element = event.currentTarget;
          const atBottom =
            element.scrollHeight - element.scrollTop - element.clientHeight <=
            FOLLOW_DISTANCE_PX;
          followRef.current = atBottom;
          if (atBottom) setUnseenOutput(false);
        }}
      >
        <div className="dm-log-timeline">
          {rows.length ? (
            rows.map((row) => <LogRowView key={row.key} row={row} />)
          ) : (
            <div className="dm-timeline-empty">
              <strong>{emptyTitle}</strong>
              <p>{emptyDetail}</p>
            </div>
          )}
        </div>
      </div>
      {unseenOutput ? (
        <button type="button" className="dm-new-output" onClick={scrollToBottom}>
          <ArrowDown size={15} aria-hidden="true" /> New output
        </button>
      ) : null}
    </div>
  );
}
