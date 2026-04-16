import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { PairingGate } from "./PairingGate";

describe("PairingGate", () => {
  it("uses desktop dashboard pairing language and omits sample tokens", () => {
    const html = renderToStaticMarkup(<PairingGate />);

    expect(html).toContain("Remote");
    expect(html).toContain("Browser Access");
    expect(html).toContain("Browser pair token");
    expect(html).toContain("Pair browser");

    expect(html).not.toContain("Browser Web UI");
    expect(html).not.toContain("GVYKXA4G");
    expect(html).not.toContain("e.g.");
  });
});
