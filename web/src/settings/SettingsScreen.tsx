import {
  Bell,
  Keyboard,
  LockKeyhole,
  MonitorSmartphone,
  MoonStar,
  Radio,
  Smartphone,
  SquareTerminal,
} from "lucide-react";
import { useEffect, useState } from "react";

import type { WsStatus } from "../api/ws";
import { isStandaloneDisplayMode } from "../app/restore";
import {
  currentNotificationAvailability,
  disablePushNotifications,
  enablePushNotifications,
  readPushRegistrationState,
  type NotificationAvailability,
} from "../pwa/notifications";
import { useDensityPreference } from "./densityPreference";
import {
  useReturnBehavior,
  useTerminalPreference,
} from "./inputPreference";

interface SettingsScreenProps {
  status: WsStatus;
}

function statusLabel(status: WsStatus): string {
  if (status.kind === "open") return "Connected automatically";
  if (status.kind === "connecting") return "Reconnecting automatically";
  if (status.kind === "closed") return "Waiting for the host";
  return "Starting";
}

type NotificationSetupState =
  | "checking"
  | "disabled"
  | "enabled"
  | "enabling"
  | "disabling"
  | "denied"
  | "error";

function unavailableNotificationLabel(
  availability: NotificationAvailability,
): string {
  if (availability.supported) return "Available after setup";
  if (availability.reason === "insecure") {
    return "Requires a secure HTTPS address";
  }
  if (availability.reason === "notInstalled") {
    return "Add to Home Screen to enable";
  }
  return "Unavailable in this browser";
}

function notificationSetupLabel(state: NotificationSetupState): string {
  switch (state) {
    case "checking":
      return "Checking notification status…";
    case "disabled":
      return "Notify me when work needs attention";
    case "enabled":
      return "Notifications are enabled";
    case "enabling":
      return "Enabling notifications…";
    case "disabling":
      return "Disabling notifications…";
    case "denied":
      return "Permission is off in iPhone Settings";
    case "error":
      return "Notification setup could not be completed";
  }
}

export function SettingsScreen({ status }: SettingsScreenProps) {
  const installed = isStandaloneDisplayMode();
  const notificationSupport = currentNotificationAvailability();
  const [notificationSetup, setNotificationSetup] =
    useState<NotificationSetupState>("checking");
  const [density, setDensity] = useDensityPreference();
  const [returnBehavior, setReturnBehavior] = useReturnBehavior();
  const [terminalPreference, setTerminalPreference] = useTerminalPreference();

  useEffect(() => {
    if (!notificationSupport.supported) return;
    let current = true;
    void readPushRegistrationState()
      .then((pushStatus) => {
        if (!current) return;
        setNotificationSetup(
          pushStatus.subscribed && Notification.permission === "granted"
            ? "enabled"
            : Notification.permission === "denied"
              ? "denied"
              : "disabled",
        );
      })
      .catch(() => {
        if (current) setNotificationSetup("error");
      });
    return () => {
      current = false;
    };
  }, [notificationSupport.reason, notificationSupport.supported]);

  const toggleNotifications = async () => {
    const disabling = notificationSetup === "enabled";
    setNotificationSetup(disabling ? "disabling" : "enabling");
    try {
      if (disabling) {
        await disablePushNotifications();
        setNotificationSetup("disabled");
      } else {
        await enablePushNotifications();
        setNotificationSetup("enabled");
      }
    } catch {
      setNotificationSetup(
        Notification.permission === "denied" ? "denied" : "error",
      );
    }
  };

  return (
    <section className="dm-screen" aria-labelledby="settings-title">
      <header className="dm-large-title-header">
        <div>
          <p className="dm-eyebrow">This iPhone</p>
          <h1 id="settings-title">Settings</h1>
        </div>
      </header>
      <div className="dm-screen-scroll">
        <section className="dm-list-section dm-list-section-first">
          <h2>Experience</h2>
          <div className="dm-grouped-list">
            <div className="dm-setting-row">
              <span className="dm-setting-icon dm-icon-blue" aria-hidden="true"><MoonStar size={18} /></span>
              <span className="dm-row-copy"><strong>Appearance</strong><small>Matches your system</small></span>
            </div>
            <label className="dm-setting-row">
              <span className="dm-setting-icon dm-icon-purple" aria-hidden="true"><MonitorSmartphone size={18} /></span>
              <span className="dm-row-copy"><strong>Interface density</strong><small>Calm is the native default</small></span>
              <select
                aria-label="Interface density"
                value={density}
                onChange={(event) => setDensity(event.currentTarget.value as typeof density)}
              >
                <option value="calm">Calm</option>
                <option value="minimal">Minimal</option>
                <option value="full">Full detail</option>
              </select>
            </label>
            <label className="dm-setting-row">
              <span className="dm-setting-icon dm-icon-green" aria-hidden="true"><Keyboard size={18} /></span>
              <span className="dm-row-copy"><strong>Return key</strong><small>The send button always works</small></span>
              <select
                aria-label="Return key behavior"
                value={returnBehavior}
                onChange={(event) => setReturnBehavior(event.currentTarget.value as typeof returnBehavior)}
              >
                <option value="newline">New line</option>
                <option value="send">Send message</option>
              </select>
            </label>
            <label className="dm-setting-row">
              <span className="dm-setting-icon dm-icon-gray" aria-hidden="true"><SquareTerminal size={18} /></span>
              <span className="dm-row-copy"><strong>Terminal view</strong><small>Advanced session display</small></span>
              <select
                aria-label="Terminal view preference"
                value={terminalPreference}
                onChange={(event) => setTerminalPreference(event.currentTarget.value as typeof terminalPreference)}
              >
                <option value="automatic">Automatic</option>
                <option value="raw">Open raw terminal</option>
              </select>
            </label>
          </div>
        </section>

        <section className="dm-list-section">
          <h2>Connection</h2>
          <div className="dm-grouped-list">
            <div className="dm-setting-row">
              <span className="dm-setting-icon dm-icon-green" aria-hidden="true"><Radio size={18} /></span>
              <span className="dm-row-copy"><strong>DevManager host</strong><small>{statusLabel(status)}</small></span>
              <span className="dm-status-word" data-online={status.kind === "open" || undefined}>
                {status.kind === "open" ? "Online" : "Waiting"}
              </span>
            </div>
            <div className="dm-setting-row">
              <span className="dm-setting-icon dm-icon-gray" aria-hidden="true"><LockKeyhole size={18} /></span>
              <span className="dm-row-copy"><strong>Host runtime</strong><small>Browser state follows the native app</small></span>
              <span className="dm-status-word">Current</span>
            </div>
          </div>
          <p className="dm-section-footnote">Sessions resume automatically while this DevManager host keeps running. A host restart begins a fresh workspace.</p>
        </section>

        <section className="dm-list-section">
          <h2>iPhone features</h2>
          <div className="dm-grouped-list">
            <div className="dm-setting-row">
              <span className="dm-setting-icon dm-icon-blue" aria-hidden="true"><Smartphone size={18} /></span>
              <span className="dm-row-copy"><strong>Home Screen app</strong><small>{installed ? "Installed" : "Open Share, then Add to Home Screen"}</small></span>
            </div>
            <div className="dm-setting-row dm-notification-row">
              <span className="dm-setting-icon dm-icon-red" aria-hidden="true"><Bell size={18} /></span>
              <span className="dm-row-copy dm-notification-copy">
                <strong>Notifications</strong>
                <small id="notification-status" aria-live="polite">
                  {notificationSupport.supported
                    ? notificationSetupLabel(notificationSetup)
                    : unavailableNotificationLabel(notificationSupport)}
                </small>
              </span>
              {notificationSupport.supported ? (
                <button
                  type="button"
                  className="dm-setting-action"
                  aria-describedby="notification-status"
                  disabled={
                    notificationSetup === "checking" ||
                    notificationSetup === "enabling" ||
                    notificationSetup === "disabling"
                  }
                  onClick={() => void toggleNotifications()}
                >
                  {notificationSetup === "enabled" ||
                  notificationSetup === "disabling"
                    ? "Disable notifications"
                    : "Enable notifications"}
                </button>
              ) : null}
            </div>
          </div>
        </section>
      </div>
    </section>
  );
}
