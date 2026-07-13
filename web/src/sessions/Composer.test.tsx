// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Composer } from "./Composer";

afterEach(cleanup);

describe("native session composer", () => {
  it("uses a native multiline textarea and sends without a resume step", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    render(
      <Composer
        value=""
        disabled={false}
        pending={false}
        supportsAttachments={false}
        onChange={onChange}
        onSubmit={onSubmit}
      />,
    );

    const textarea = screen.getByRole("textbox", { name: /message/i });
    expect(textarea.tagName).toBe("TEXTAREA");
    expect(textarea.getAttribute("enterkeyhint")).toBe("send");

    await user.type(textarea, "hello from dictation");
    expect(onChange).toHaveBeenLastCalledWith("hello from dictation");
    await user.click(screen.getByRole("button", { name: /send/i }));

    expect(onSubmit).toHaveBeenCalledWith("hello from dictation", []);
  });

  it("keeps Return multiline-safe and sends on modified Return", async () => {
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    render(
      <Composer
        value="line one"
        disabled={false}
        pending={false}
        supportsAttachments={false}
        onChange={() => {}}
        onSubmit={onSubmit}
      />,
    );
    const textarea = screen.getByRole("textbox", { name: /message/i });

    fireEvent.keyDown(textarea, { key: "Enter", shiftKey: true });
    expect(onSubmit).not.toHaveBeenCalled();
    fireEvent.keyDown(textarea, { key: "Enter", metaKey: true });

    expect(onSubmit).toHaveBeenCalledWith("line one", []);
  });

  it("validates and previews native image attachments", async () => {
    const user = userEvent.setup();
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    render(
      <Composer
        value="look at this"
        disabled={false}
        pending={false}
        supportsAttachments
        onChange={() => {}}
        onSubmit={onSubmit}
      />,
    );

    const picker = screen.getByLabelText(/attach image/i);
    expect(picker.getAttribute("accept")).toBe("image/png,image/jpeg");
    const file = new File([new Uint8Array([1, 2, 3])], "screen.png", { type: "image/png" });
    await user.upload(picker, file);

    expect(screen.getByText("screen.png").isConnected).toBe(true);
    await user.click(screen.getByRole("button", { name: /send/i }));
    await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(1));
    expect(onSubmit.mock.calls[0][1]).toHaveLength(1);
    expect(onSubmit.mock.calls[0][1][0]).toMatchObject({ mimeType: "image/png", fileName: "screen.png" });
  });

  it("disables mutations while reconnecting without hiding the draft", () => {
    render(
      <Composer
        value="preserved draft"
        disabled
        pending={false}
        supportsAttachments
        onChange={() => {}}
        onSubmit={async () => {}}
      />,
    );

    expect((screen.getByDisplayValue("preserved draft") as HTMLTextAreaElement).disabled).toBe(true);
    expect(screen.getByText(/reconnecting/i).isConnected).toBe(true);
    expect((screen.getByRole("button", { name: /send/i }) as HTMLButtonElement).disabled).toBe(true);
  });
});
