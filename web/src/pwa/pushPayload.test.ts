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
        ignored: "not copied",
      }),
    ).toEqual({
      title: "Build finished",
      body: "Ready for review",
      route: "/sessions?filter=attention",
      tag: "session-alert",
    });
    expect(parsePushPayload({ title: 7, route: [] })).toEqual({});
  });
});
