// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { MarkdownMessage } from "./MarkdownMessage";

afterEach(cleanup);

describe("safe native Markdown message", () => {
  it("renders GFM structure and keeps wide content locally scrollable", () => {
    const { container } = render(
      <MarkdownMessage
        text={[
          "## Result",
          "",
          "- [x] Fixed monitor",
          "- [ ] Ship it",
          "",
          "| Project | Type |",
          "| --- | --- |",
          "| Portal | Codex |",
          "",
          "```ts",
          "const ready = true;",
          "```",
        ].join("\n")}
      />,
    );

    expect(screen.getByRole("heading", { name: "Result" }).isConnected).toBe(true);
    expect(screen.getAllByRole("checkbox")[0]?.hasAttribute("checked")).toBe(true);
    expect(screen.getByRole("table").isConnected).toBe(true);
    expect(container.querySelector("pre")?.textContent).toContain("const ready = true");
  });

  it("opens external links safely and never activates raw HTML", () => {
    const { container } = render(
      <MarkdownMessage text={'[Docs](https://example.com)\n\n<img src=x onerror="alert(1)">'} />,
    );

    const link = screen.getByRole("link", { name: "Docs" });
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toBe("noreferrer noopener");
    expect(container.querySelector("img")).toBeNull();
  });
});
