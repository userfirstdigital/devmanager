export interface FingerprintEntry {
  path: string;
  contents: Uint8Array;
}

const encoder = new TextEncoder();

export function fingerprintEntries(
  entries: readonly FingerprintEntry[],
): string {
  let hash = 0xcbf29ce484222325n;
  const update = (bytes: Uint8Array) => {
    for (let index = 0; index < bytes.length; index += 1) {
      let byte = bytes[index];
      if (byte === 13 && bytes[index + 1] === 10) {
        byte = 10;
        index += 1;
      }
      hash ^= BigInt(byte);
      hash = BigInt.asUintN(64, hash * 0x100000001b3n);
    }
  };

  for (const entry of [...entries].sort((left, right) =>
    left.path < right.path ? -1 : left.path > right.path ? 1 : 0,
  )) {
    update(encoder.encode(entry.path.replace(/\\/g, "/")));
    update(new Uint8Array([0]));
    update(entry.contents);
    update(new Uint8Array([0]));
  }

  return hash.toString(16).padStart(16, "0");
}
