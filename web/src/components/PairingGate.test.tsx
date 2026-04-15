import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { PairingGate } from "./PairingGate";

describe("PairingGate", () => {
  it("does not show a sample pairing token", () => {
    const html = renderToStaticMarkup(<PairingGate />);

    expect(html).not.toContain("GVYKXA4G");
    expect(html).not.toContain("e.g.");
  });
});
