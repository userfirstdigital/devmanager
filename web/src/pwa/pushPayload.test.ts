import { describe, expect, it } from "vitest";
import { parsePushPayload } from "./pushPayload";

describe("parsePushPayload", () => {
  it.each([null, [], "message", 42, true])(
    "turns non-object parsed JSON into an empty payload: %o",
    (value) => {
      expect(parsePushPayload(value)).toEqual({});
    },
  );

  it("keeps only supported string fields from a plain object", () => {
    expect(
      parsePushPayload({
        title: "Build finished",
        body: "Ready for review",
        route: "/sessions?filter=attention",
        tag: "session-alert",
        eventId: "event-12",
        runtimeInstanceId: "runtime-3",
        action: "needsInput",
        badge: 2,
        prompt: "PROMPT_SENTINEL",
        code: "OUTPUT_SENTINEL",
        ignored: "not copied",
      }),
    ).toEqual({
      title: "Build finished",
      body: "Ready for review",
      route: "/sessions?filter=attention",
      tag: "session-alert",
      eventId: "event-12",
      runtimeInstanceId: "runtime-3",
      action: "needsInput",
      badge: 2,
    });
    expect(
      parsePushPayload({
        title: 7,
        route: [],
        badge: -1,
        action: "arbitrary",
      }),
    ).toEqual({});
  });
});
