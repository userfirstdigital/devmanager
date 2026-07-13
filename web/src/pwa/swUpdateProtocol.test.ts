import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

describe("service-worker update protocol wiring", () => {
  it("routes activation and ACK frames through the cross-tab gate", () => {
    const source = readFileSync(new URL("../sw.ts", import.meta.url), "utf8");

    expect(source).toContain("createWorkerUpdateGate");
    expect(source).toContain("isUpdateActivationRequest");
    expect(source).toContain("isUpdateSafetyAck");
    expect(source).toContain("UPDATE_ACTIVATION_RESULT");
    expect(source).not.toContain('event.data?.type === "SKIP_WAITING"');
  });
});
