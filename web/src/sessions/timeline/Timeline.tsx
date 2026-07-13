import { useLayoutEffect, useMemo, useRef } from "react";

import type { SemanticEvent } from "../../api/types";
import {
  EventRenderer,
  visibleEventsForDensity,
  type InterfaceDensity,
} from "./eventRenderers";

const FOLLOW_DISTANCE_PX = 96;

function coalesceOutput(events: SemanticEvent[]): SemanticEvent[] {
  const result: SemanticEvent[] = [];
  for (const event of events) {
    const previous = result[result.length - 1];
    if (
      previous?.kind === "output" &&
      event.kind === "output" &&
      previous.source === event.source &&
      previous.stream === event.stream
    ) {
      result[result.length - 1] = {
        ...event,
        text: `${previous.text}${event.text}`,
      };
    } else {
      result.push(event);
    }
  }
  return result;
}

export function Timeline({
  events,
  density,
  emptyTitle = "Nothing here yet",
  emptyDetail = "New activity will appear here automatically.",
}: {
  events: SemanticEvent[];
  density: InterfaceDensity;
  emptyTitle?: string;
  emptyDetail?: string;
}) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const followRef = useRef(true);
  const visibleEvents = useMemo(
    () => coalesceOutput(visibleEventsForDensity(events, density)),
    [density, events],
  );

  useLayoutEffect(() => {
    const element = scrollRef.current;
    if (element && followRef.current) element.scrollTop = element.scrollHeight;
  }, [visibleEvents]);

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
        {visibleEvents.length ? (
          visibleEvents.map((event) => (
            <EventRenderer key={`${event.stableSessionKey}:${event.sequence}`} event={event} density={density} />
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
