// @vitest-environment jsdom

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SettingsScreen } from "./SettingsScreen";

const requestPermission = vi.fn(async () => "granted" as NotificationPermission);
const subscribe = vi.fn(async () => ({
  endpoint: "https://web.push.apple.com/Q1/settings-test",
  options: { applicationServerKey: null },
  getKey: (name: PushEncryptionKeyName) =>
    new Uint8Array(name === "p256dh" ? 65 : 16).buffer,
  unsubscribe: vi.fn(async () => true),
}));
const getSubscription = vi.fn(async () => null);
const vapidPublicKey = btoa(
  String.fromCharCode(
    ...Uint8Array.from(
      { length: 65 },
      (_, index) => (index === 0 ? 4 : index),
    ),
  ),
)
  .replace(/\+/gu, "-")
  .replace(/\//gu, "_")
  .replace(/=+$/u, "");
const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
  if (String(input) === "/api/push" && (!init?.method || init.method === "GET")) {
    return new Response(
      JSON.stringify({ publicKey: vapidPublicKey, subscribed: false }),
      { status: 200, headers: { "content-type": "application/json" } },
    );
  }
  return new Response(null, { status: 204 });
});

beforeEach(() => {
  Object.defineProperty(globalThis, "isSecureContext", {
    configurable: true,
    value: true,
  });
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: vi.fn(() => ({ matches: true })),
  });
  Object.defineProperty(navigator, "serviceWorker", {
    configurable: true,
    value: {
      ready: Promise.resolve({ pushManager: { getSubscription, subscribe } }),
    },
  });
  vi.stubGlobal("PushManager", class PushManager {});
  vi.stubGlobal("Notification", {
    permission: "default",
    requestPermission,
  });
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
  vi.unstubAllGlobals();
});

describe("notification settings", () => {
  it("never requests permission until the accessible Enable button is clicked", async () => {
    const user = userEvent.setup();
    render(<SettingsScreen status={{ kind: "open" }} />);

    const button = await screen.findByRole("button", {
      name: "Enable notifications",
    });
    expect(requestPermission).not.toHaveBeenCalled();
    expect(button.getAttribute("type")).toBe("button");

    await user.click(button);

    await waitFor(() => expect(requestPermission).toHaveBeenCalledTimes(1));
    expect(await screen.findByText("Notifications are enabled")).toBeTruthy();
  });

  it("shows HTTPS guidance without exposing a permission action on an insecure page", () => {
    Object.defineProperty(globalThis, "isSecureContext", {
      configurable: true,
      value: false,
    });

    render(<SettingsScreen status={{ kind: "open" }} />);

    expect(screen.queryByRole("button", { name: /notifications/i })).toBeNull();
    expect(screen.getByText("Requires a secure HTTPS address")).toBeTruthy();
    expect(requestPermission).not.toHaveBeenCalled();
  });
});
