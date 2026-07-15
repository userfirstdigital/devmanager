import { useLayoutEffect, useMemo, useRef } from "react";

import type { SemanticEvent } from "../../api/types";
import {
  ConversationItemRenderer,
  type InterfaceDensity,
} from "./eventRenderers";
import { buildConversationItems } from "./timelineModel";

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
}: {
  events: SemanticEvent[];
  density: InterfaceDensity;
  includeFallbackOutput?: boolean;
  emptyTitle?: string;
  emptyDetail?: string;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const followRef = useRef(true);
  const visibleAnchorRef = useRef<VisibleAnchor | null>(null);
  const visibleItems = useMemo(
    () => buildConversationItems(events, density, includeFallbackOutput),
    [density, events, includeFallbackOutput],
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
            <ConversationItemRenderer key={item.key} item={item} density={density} />
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
