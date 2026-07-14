// @vitest-environment jsdom

import { cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useStore } from "../../store";
import { RawTerminalView } from "./RawTerminalView";

vi.mock("../../components/Terminal", () => ({
  TerminalView: () => <div>Terminal</div>,
}));
vi.mock("../../components/MobileKeyRow", () => ({
  MobileKeyRow: () => <div>Keys</div>,
}));

afterEach(cleanup);

describe("raw terminal stream ownership", () => {
  it("subscribes on mount and clears the raw stream on unmount", () => {
    const setRawTerminalSession = vi.fn();
    useStore.setState({ setRawTerminalSession });

    const view = render(<RawTerminalView sessionId="pty-a" />);

    expect(setRawTerminalSession).toHaveBeenCalledWith("pty-a");

    view.unmount();

    expect(setRawTerminalSession).toHaveBeenLastCalledWith(null);
    expect(setRawTerminalSession).toHaveBeenCalledTimes(2);
  });
});
