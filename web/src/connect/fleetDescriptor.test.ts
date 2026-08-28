import { describe, expect, it } from "vitest";

import {
  parseConnectFleetDescriptors,
  CONNECT_FLEET_MAX_HOSTS,
} from "./fleetDescriptor";
import {
  parseScopedHostTaskKey,
  scopedHostTaskKey,
  scopeHostTask,
} from "./scopedHostTask";

const PAGE = {
  hostPublicId: "01234567-89ab-7000-8000-000000000001",
  hostPublicKey: "aa".repeat(32),
  origin: "http://127.0.0.1:8787",
  generation: 3,
  protocolMajor: 1,
  protocolMinor: 0,
  label: "Page host",
};

const REMOTE = {
  hostPublicId: "01234567-89ab-7000-8000-000000000002",
  hostPublicKey: "bb".repeat(32),
  origin: "https://studio.example",
  label: "Studio",
  generation: 2,
  protocolMajor: 1,
  protocolMinor: 0,
};

describe("scopedHostTask", () => {
  it("builds stable host:task keys without collapsing domain UUIDs", () => {
    const left = scopeHostTask(PAGE.hostPublicId, "01234567-89ab-7000-8000-0000000000aa")!;
    const right = scopeHostTask(REMOTE.hostPublicId, "01234567-89ab-7000-8000-0000000000aa")!;
    expect(scopedHostTaskKey(left)).not.toBe(scopedHostTaskKey(right));
    expect(parseScopedHostTaskKey(scopedHostTaskKey(left))).toEqual(left);
  });
});

describe("parseConnectFleetDescriptors", () => {
  it("keeps the page host when fleet metadata is absent", () => {
    const result = parseConnectFleetDescriptors({ pageHost: PAGE, fleetJson: null });
    expect(result.hosts).toHaveLength(1);
    expect(result.hosts[0]!.isPageHost).toBe(true);
    expect(result.heldAdditions).toBe(false);
  });

  it("holds malformed fleet additions without legacy fallback", () => {
    const result = parseConnectFleetDescriptors({
      pageHost: PAGE,
      fleetJson: { version: 1, hosts: [{ ...REMOTE, origin: "http://insecure.example" }] },
    });
    expect(result.hosts).toHaveLength(1);
    expect(result.heldAdditions).toBe(true);
    expect(result.holdReason).toBe("malformed");
  });

  it("holds duplicate host id retargets instead of silently changing origin/key", () => {
    const result = parseConnectFleetDescriptors({
      pageHost: PAGE,
      fleetJson: {
        version: 1,
        hosts: [
          {
            ...PAGE,
            origin: "https://evil.example",
            hostPublicKey: "cc".repeat(32),
            label: "Retarget",
          },
        ],
      },
    });
    expect(result.hosts).toHaveLength(1);
    expect(result.holdReason).toBe("duplicate_host_retarget");
  });

  it("accepts HTTPS remote hosts up to capacity including the page host", () => {
    const hosts = Array.from({ length: CONNECT_FLEET_MAX_HOSTS - 1 }, (_, index) => ({
      ...REMOTE,
      hostPublicId: `01234567-89ab-7000-8000-${String(index + 2).padStart(12, "0")}`,
      hostPublicKey: `${(index + 2).toString(16).padStart(2, "0")}`.repeat(32),
      origin: `https://h${index}.example`,
      label: `Host ${index}`,
    }));
    const result = parseConnectFleetDescriptors({
      pageHost: PAGE,
      fleetJson: { version: 1, hosts },
    });
    expect(result.hosts).toHaveLength(CONNECT_FLEET_MAX_HOSTS);
    expect(result.heldAdditions).toBe(false);
  });
});
