// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";

import { loadDensity, saveDensity } from "./densityPreference";

describe("interface density preference", () => {
  beforeEach(() => localStorage.clear());

  it("defaults to Calm and offers Minimal and Full as durable alternatives", () => {
    expect(loadDensity()).toBe("calm");
    saveDensity("minimal");
    expect(loadDensity()).toBe("minimal");
    saveDensity("full");
    expect(loadDensity()).toBe("full");
  });

  it("fails closed to Calm for unknown persisted values", () => {
    localStorage.setItem("devmanager-interface-density:v1", "noisy");
    expect(loadDensity()).toBe("calm");
  });
});
