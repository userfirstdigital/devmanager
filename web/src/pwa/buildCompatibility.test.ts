import { describe, expect, it } from "vitest";
import {
  CLIENT_WEB_BUILD_ID,
  evaluateBuildCompatibility,
} from "./buildCompatibility";

describe("evaluateBuildCompatibility", () => {
  it("accepts the host that serves this exact browser bundle", () => {
    expect(evaluateBuildCompatibility(CLIENT_WEB_BUILD_ID)).toEqual({
      kind: "compatible",
    });
  });

  it("requires a reload when the native host embeds a different bundle", () => {
    expect(evaluateBuildCompatibility("different-host-build")).toEqual({
      kind: "reloadRequired",
      clientBuildId: CLIENT_WEB_BUILD_ID,
      hostBuildId: "different-host-build",
    });
  });
});
