import { describe, expect, it } from "vitest";

import {
  HostTrustHoldError,
  assertHostTrust,
  type HostTrustRecord,
  type HostTrustStorage,
} from "./hostTrust";

const first = {
  origin: "https://phone.example.test",
  hostPublicId: "01234567-89ab-7000-8000-000000000017",
  hostPublicKey: "ab".repeat(32),
};

function storage(): HostTrustStorage & { records: HostTrustRecord[] } {
  const records: HostTrustRecord[] = [];
  return {
    records,
    async pin(record) {
      const prior = records.find(
        (candidate) =>
          candidate.origin === record.origin &&
          candidate.hostPublicId === record.hostPublicId,
      );
      if (prior) return prior;
      records.push(record);
      return record;
    },
  };
}

describe("host trust pinning", () => {
  it("pins public host metadata on first paired bootstrap", async () => {
    const durable = storage();
    await expect(assertHostTrust(first, { storage: durable })).resolves.toEqual(
      first,
    );
    expect(durable.records).toEqual([first]);
    expect(JSON.stringify(durable.records)).not.toContain("private");
  });

  it("holds a changed key and leaves the durable first pin intact", async () => {
    const durable = storage();
    await assertHostTrust(first, { storage: durable });
    await expect(
      assertHostTrust({ ...first, hostPublicKey: "cd".repeat(32) }, { storage: durable }),
    ).rejects.toBeInstanceOf(HostTrustHoldError);
    expect(durable.records).toEqual([first]);
  });

  it("treats a concurrent first-pin winner as authoritative", async () => {
    const winner = { ...first, hostPublicKey: "ef".repeat(32) };
    const durable: HostTrustStorage = {
      pin: async () => winner,
    };
    await expect(assertHostTrust(first, { storage: durable })).rejects.toBeInstanceOf(
      HostTrustHoldError,
    );
  });

  it("fails closed when durable storage is unavailable", async () => {
    const durable: HostTrustStorage = {
      pin: async () => {
        throw new Error("quota denied");
      },
    };
    await expect(assertHostTrust(first, { storage: durable })).rejects.toBeInstanceOf(
      HostTrustHoldError,
    );
  });
});
