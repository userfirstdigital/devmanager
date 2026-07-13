import {
  Bell,
  LockKeyhole,
  MonitorSmartphone,
  MoonStar,
  Radio,
  Smartphone,
} from "lucide-react";

import type { WsStatus } from "../api/ws";
import { isStandaloneDisplayMode } from "../app/restore";

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
            <div className="dm-setting-row">
              <span className="dm-setting-icon dm-icon-purple" aria-hidden="true"><MonitorSmartphone size={18} /></span>
              <span className="dm-row-copy"><strong>Interface density</strong><small>Calm</small></span>
            </div>
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
