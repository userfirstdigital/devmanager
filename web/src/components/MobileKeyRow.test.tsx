// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useStore } from "../store";
import { MobileKeyRow } from "./MobileKeyRow";

afterEach(cleanup);

describe("mobile terminal input provenance", () => {
  it("sends helper keys as UTF-8 Bytes frames", async () => {
    const user = userEvent.setup();
    const sendInput = vi.fn();
    useStore.setState({ sendInput });
    render(<MobileKeyRow sessionId="pty-mobile" />);

    await user.click(screen.getByRole("button", { name: "Enter" }));
    await user.click(screen.getByRole("button", { name: "Slash" }));

    expect(sendInput).toHaveBeenNthCalledWith(1, "pty-mobile", "\r", "bytes");
    expect(sendInput).toHaveBeenNthCalledWith(2, "pty-mobile", "/", "bytes");
  });
});
