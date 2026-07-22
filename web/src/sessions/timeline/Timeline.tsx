import { useLayoutEffect, useMemo, useRef } from "react";

import type { SemanticEvent } from "../../api/types";
import {
  ConversationItemRenderer,
  type InterfaceDensity,
} from "./eventRenderers";
import { buildConversationItems, type ConversationItem } from "./timelineModel";

const FOLLOW_DISTANCE_PX = 96;

interface TimelineAnchorCalculation {
  scrollTop: number;
  previousAnchorOffset: number;
  nextAnchorOffset: number;
}

export function preserveTimelineAnchor({
  scrollTop,
  previousAnchorOffset,
  nextAnchorOffset,
}: TimelineAnchorCalculation): number {
  return Math.max(0, scrollTop + nextAnchorOffset - previousAnchorOffset);
}

/** Latest question with no subsequent user message is the only interactive one. */
export function actionableQuestionKey(items: ConversationItem[]): string | null {
  let latest: string | null = null;
  for (const item of items) {
    if (item.kind === "message" && item.role === "user") {
      latest = null;
      continue;
    }
    if (item.kind === "question") {
      latest = item.key;
    }
  }
  return latest;
}

interface VisibleAnchor {
  sequence: string;
  offset: number;
}

function captureVisibleAnchor(element: HTMLDivElement): VisibleAnchor | null {
  const viewportTop = element.getBoundingClientRect().top;
  const candidates = element.querySelectorAll<HTMLElement>("[data-event-sequence]");
  for (const candidate of candidates) {
    const rect = candidate.getBoundingClientRect();
    if (rect.bottom >= viewportTop) {
      const sequence = candidate.dataset.eventSequence;
      if (sequence) return { sequence, offset: rect.top - viewportTop };
    }
  }
  return null;
}

export function Timeline({
  events,
  density,
  includeFallbackOutput = true,
  emptyTitle = "Nothing here yet",
  emptyDetail = "New activity will appear here automatically.",
  onQuestionChoice,
  questionChoicesDisabled = false,
}: {
  events: SemanticEvent[];
  density: InterfaceDensity;
  includeFallbackOutput?: boolean;
  emptyTitle?: string;
  emptyDetail?: string;
  onQuestionChoice?(choice: string): void;
  questionChoicesDisabled?: boolean;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const followRef = useRef(true);
  const visibleAnchorRef = useRef<VisibleAnchor | null>(null);
  const visibleItems = useMemo(
    () => buildConversationItems(events, density, includeFallbackOutput),
    [density, events, includeFallbackOutput],
  );
  const activeQuestionKey = useMemo(
    () => actionableQuestionKey(visibleItems),
    [visibleItems],
  );

  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (!element) return;
    const previousAnchor = visibleAnchorRef.current;
    visibleAnchorRef.current = null;
    if (followRef.current) {
      element.scrollTop = element.scrollHeight;
    } else if (previousAnchor) {
      const anchored = element.querySelector<HTMLElement>(
        `[data-event-sequence="${previousAnchor.sequence}"]`,
      );
      if (anchored) {
        const nextOffset =
          anchored.getBoundingClientRect().top - element.getBoundingClientRect().top;
        element.scrollTop = preserveTimelineAnchor({
          scrollTop: element.scrollTop,
          previousAnchorOffset: previousAnchor.offset,
          nextAnchorOffset: nextOffset,
        });
      }
    }
    return () => {
      if (!followRef.current) {
        visibleAnchorRef.current = captureVisibleAnchor(element);
      }
    };
  }, [visibleItems]);

  return (
    <div
      ref={scrollRef}
      className="dm-timeline-scroll"
      role="log"
      aria-live="polite"
      aria-relevant="additions text"
      onScroll={(event) => {
        const element = event.currentTarget;
        followRef.current =
          element.scrollHeight - element.scrollTop - element.clientHeight <= FOLLOW_DISTANCE_PX;
      }}
    >
      <div className="dm-timeline">
        {visibleItems.length ? (
          visibleItems.map((item) => (
            <ConversationItemRenderer
              key={item.key}
              item={item}
              density={density}
              onQuestionChoice={onQuestionChoice}
              questionChoicesDisabled={
                questionChoicesDisabled ||
                (item.kind === "question" && item.key !== activeQuestionKey)
              }
            />
          ))
        ) : (
          <div className="dm-timeline-empty">
            <strong>{emptyTitle}</strong>
            <p>{emptyDetail}</p>
          </div>
        )}
      </div>
    </div>
  );
}
