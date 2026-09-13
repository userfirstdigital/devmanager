import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { PairingGate } from "./PairingGate";

describe("PairingGate", () => {
  it("points to the native pairing settings and omits sample tokens", () => {
    const html = renderToStaticMarkup(<PairingGate />);

    expect(html).toContain("Settings → Remote access → Pair a device");
    expect(html).toContain("Pairing code");
    expect(html).toContain("Pair browser");

    expect(html).not.toContain("Browser Web UI");
    expect(html).not.toContain("GVYKXA4G");
    expect(html).not.toContain("e.g.");
  });
});
