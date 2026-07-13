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

import type { WsStatus } from "../api/ws";
import { isStandaloneDisplayMode } from "../app/restore";
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

export function SettingsScreen({ status }: SettingsScreenProps) {
  const secure = globalThis.isSecureContext === true;
  const installed = isStandaloneDisplayMode();
  const [density, setDensity] = useDensityPreference();
  const [returnBehavior, setReturnBehavior] = useReturnBehavior();
  const [terminalPreference, setTerminalPreference] = useTerminalPreference();

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
            <div className="dm-setting-row">
              <span className="dm-setting-icon dm-icon-red" aria-hidden="true"><Bell size={18} /></span>
              <span className="dm-row-copy"><strong>Notifications</strong><small>{secure ? "Available after setup" : "Requires a secure HTTPS address"}</small></span>
            </div>
          </div>
        </section>
      </div>
    </section>
  );
}
