import { describe, expect, it } from "vitest";
import { fingerprintEntries } from "./sourceFingerprint";

const encoder = new TextEncoder();

describe("fingerprintEntries", () => {
  it("produces the same fingerprint for LF and CRLF source content", () => {
    const lf = fingerprintEntries([
      {
        path: "src/example.ts",
        contents: encoder.encode("const first = 1;\nconst second = 2;\n"),
      },
    ]);
    const crlf = fingerprintEntries([
      {
        path: "src/example.ts",
        contents: encoder.encode("const first = 1;\r\nconst second = 2;\r\n"),
      },
    ]);

    expect(crlf).toBe(lf);
    expect(lf).toMatch(/^[0-9a-f]{16}$/);
  });
});
