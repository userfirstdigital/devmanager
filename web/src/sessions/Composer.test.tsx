// @vitest-environment jsdom

import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { Composer } from "./Composer";

afterEach(cleanup);

function deferred<T>() {
  let resolve: (value: T) => void = () => {};
  const promise = new Promise<T>((resolvePromise) => {
    resolve = resolvePromise;
  });
  return { promise, resolve };
}

describe("native session composer", () => {
  it("opens the provider command list from a slash and accepts keyboard selection without submitting", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    render(
      <Composer
        scopeKey="runtime-a:tab:claude-a"
        value=""
        disabled={false}
        pending={false}
        supportsAttachments={false}
        provider="claude"
        catalogSessionKey="tab:claude-a"
        onChange={onChange}
        onSubmit={onSubmit}
      />,
    );
    const textarea = screen.getByRole("textbox", { name: /message/i });
    await user.type(textarea, "/mod");

    expect(screen.getByRole("listbox", { name: /claude commands/i })).toBeTruthy();
    expect(screen.getByRole("option", { name: /^\/model/i })).toBeTruthy();
    fireEvent.keyDown(textarea, { key: "Enter" });

    expect(onChange).toHaveBeenLastCalledWith("/model");
    expect(onSubmit).not.toHaveBeenCalled();
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("closes slash suggestions on Escape and never opens them for non-AI sessions", async () => {
    const user = userEvent.setup();
    const { rerender } = render(
      <Composer
        scopeKey="runtime-a:tab:codex-a"
        value="/"
        disabled={false}
        pending={false}
        supportsAttachments={false}
        provider="codex"
        catalogSessionKey="tab:codex-a"
        onChange={() => {}}
        onSubmit={async () => {}}
      />,
    );
    expect(screen.getByRole("listbox", { name: /codex commands/i })).toBeTruthy();
    fireEvent.keyDown(screen.getByRole("textbox", { name: /message/i }), {
      key: "Escape",
    });
    expect(screen.queryByRole("listbox")).toBeNull();

    rerender(
      <Composer
        scopeKey="runtime-a:server:shell-a"
        value="/"
        disabled={false}
        pending={false}
        supportsAttachments={false}
        onChange={() => {}}
        onSubmit={async () => {}}
      />,
    );
    await user.click(screen.getByRole("textbox", { name: /message/i }));
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("offers stable native argument suggestions and keeps the sheet visible while reconnecting", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const { rerender } = render(
      <Composer
        scopeKey="runtime-a:tab:claude-a"
        value="/fast"
        disabled={false}
        pending={false}
        supportsAttachments={false}
        provider="claude"
        catalogSessionKey="tab:claude-a"
        onChange={onChange}
        onSubmit={async () => {}}
      />,
    );
    await user.click(screen.getByRole("option", { name: /^\/fast/i }));
    expect(screen.getByRole("button", { name: /use on/i })).toBeTruthy();
    await user.click(screen.getByRole("button", { name: /use on/i }));
    expect(onChange).toHaveBeenLastCalledWith("/fast on");

    rerender(
      <Composer
        scopeKey="runtime-a:tab:claude-b"
        value="/comp"
        disabled
        pending={false}
        supportsAttachments={false}
        provider="claude"
        catalogSessionKey="tab:claude-b"
        onChange={() => {}}
        onSubmit={async () => {}}
      />,
    );
    expect(screen.getByDisplayValue("/comp")).toBeTruthy();
    expect(screen.getByRole("listbox", { name: /claude commands/i })).toBeTruthy();
    expect(screen.getByText(/reconnecting/i)).toBeTruthy();
  });
  it("uses a native multiline textarea and sends without a resume step", async () => {
    const user = userEvent.setup();
    const onChange = vi.fn();
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    render(
      <Composer
        scopeKey="runtime-a:tab:claude-a"
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
        scopeKey="runtime-a:tab:claude-a"
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
        scopeKey="runtime-a:tab:claude-a"
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
        scopeKey="runtime-a:tab:claude-a"
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

  it("sends on plain Return only after the user chooses that preference", () => {
    const onSubmit = vi.fn().mockResolvedValue(undefined);
    render(
      <Composer
        scopeKey="runtime-a:tab:claude-a"
        value="send this"
        disabled={false}
        pending={false}
        supportsAttachments={false}
        returnBehavior="send"
        onChange={() => {}}
        onSubmit={onSubmit}
      />,
    );
    fireEvent.keyDown(screen.getByRole("textbox", { name: /message/i }), {
      key: "Enter",
    });
    expect(onSubmit).toHaveBeenCalledWith("send this", []);
  });

  it("clears attachment and in-flight UI state when the session scope changes", async () => {
    const user = userEvent.setup();
    let resolveFirstSubmit: () => void = () => {};
    const firstSubmit = vi.fn(
      () =>
        new Promise<void>((resolve) => {
          resolveFirstSubmit = resolve;
        }),
    );
    const secondSubmit = vi.fn().mockResolvedValue(undefined);
    const { rerender } = render(
      <Composer
        scopeKey="runtime-a:tab:claude-a"
        value="message for A"
        disabled={false}
        pending={false}
        supportsAttachments
        onChange={() => {}}
        onSubmit={firstSubmit}
      />,
    );

    await user.upload(
      screen.getByLabelText(/attach image/i),
      new File([new Uint8Array([1, 2, 3])], "a-only.png", {
        type: "image/png",
      }),
    );
    await user.click(screen.getByRole("button", { name: /send message/i }));
    expect(firstSubmit).toHaveBeenCalledTimes(1);

    rerender(
      <Composer
        scopeKey="runtime-a:tab:claude-b"
        value="message for B"
        disabled={false}
        pending={false}
        supportsAttachments
        onChange={() => {}}
        onSubmit={secondSubmit}
      />,
    );

    await waitFor(() => {
      expect(screen.queryByText("a-only.png")).toBeNull();
      expect(
        (screen.getByRole("textbox", { name: /message/i }) as HTMLTextAreaElement)
          .value,
      ).toBe("message for B");
      expect(
        (screen.getByRole("button", { name: /send message/i }) as HTMLButtonElement)
          .disabled,
      ).toBe(false);
    });

    resolveFirstSubmit();
    await waitFor(() => {
      expect(
        (screen.getByRole("textbox", { name: /message/i }) as HTMLTextAreaElement)
          .value,
      ).toBe("message for B");
    });
  });

  it("does not republish an attachment read after its session scope changes", async () => {
    const read = deferred<ArrayBuffer>();
    const file = new File([new Uint8Array([1, 2, 3])], "a-only.png", {
      type: "image/png",
    });
    Object.defineProperty(file, "arrayBuffer", {
      value: vi.fn(() => read.promise),
    });
    const safetyStates: Array<{
      selectedAttachments: number;
      attachmentLoads: number;
    }> = [];
    const onSafetyStateChange = vi.fn(
      (state: { selectedAttachments: number; attachmentLoads: number }) => {
        safetyStates.push({ ...state });
      },
    );
    const { rerender } = render(
      <Composer
        scopeKey="runtime-a:tab:claude-a"
        value=""
        disabled={false}
        pending={false}
        supportsAttachments
        onChange={() => {}}
        onSubmit={async () => {}}
        onSafetyStateChange={onSafetyStateChange}
      />,
    );

    fireEvent.change(screen.getByLabelText(/attach image/i), {
      target: { files: [file] },
    });
    await waitFor(() =>
      expect(safetyStates[safetyStates.length - 1]).toEqual({
        selectedAttachments: 0,
        attachmentLoads: 1,
      }),
    );

    rerender(
      <Composer
        scopeKey="runtime-a:tab:claude-b"
        value=""
        disabled={false}
        pending={false}
        supportsAttachments
        onChange={() => {}}
        onSubmit={async () => {}}
        onSafetyStateChange={onSafetyStateChange}
      />,
    );
    expect(safetyStates[safetyStates.length - 1]).toEqual({
      selectedAttachments: 0,
      attachmentLoads: 0,
    });

    await act(async () => {
      read.resolve(new ArrayBuffer(3));
      await read.promise;
    });
    expect(safetyStates[safetyStates.length - 1]).toEqual({
      selectedAttachments: 0,
      attachmentLoads: 0,
    });
  });

  it("clears safety and ignores an attachment read that finishes after unmount", async () => {
    const read = deferred<ArrayBuffer>();
    const file = new File([new Uint8Array([1, 2, 3])], "a-only.png", {
      type: "image/png",
    });
    Object.defineProperty(file, "arrayBuffer", {
      value: vi.fn(() => read.promise),
    });
    const safetyStates: Array<{
      selectedAttachments: number;
      attachmentLoads: number;
    }> = [];
    const { unmount } = render(
      <Composer
        scopeKey="runtime-a:tab:claude-a"
        value=""
        disabled={false}
        pending={false}
        supportsAttachments
        onChange={() => {}}
        onSubmit={async () => {}}
        onSafetyStateChange={(state) => safetyStates.push({ ...state })}
      />,
    );

    fireEvent.change(screen.getByLabelText(/attach image/i), {
      target: { files: [file] },
    });
    await waitFor(() =>
      expect(safetyStates[safetyStates.length - 1]).toEqual({
        selectedAttachments: 0,
        attachmentLoads: 1,
      }),
    );

    unmount();
    expect(safetyStates[safetyStates.length - 1]).toEqual({
      selectedAttachments: 0,
      attachmentLoads: 0,
    });

    await act(async () => {
      read.resolve(new ArrayBuffer(3));
      await read.promise;
    });
    expect(safetyStates[safetyStates.length - 1]).toEqual({
      selectedAttachments: 0,
      attachmentLoads: 0,
    });
  });

  it("rejects an oversized selection before reading any image bytes", async () => {
    const user = userEvent.setup();
    const first = new File([new Uint8Array(4 * 1024 * 1024)], "first.png", {
      type: "image/png",
    });
    const second = new File([new Uint8Array(2 * 1024 * 1024)], "second.png", {
      type: "image/png",
    });
    const firstRead = vi.fn().mockResolvedValue(new ArrayBuffer(0));
    const secondRead = vi.fn().mockResolvedValue(new ArrayBuffer(0));
    Object.defineProperty(first, "arrayBuffer", { value: firstRead });
    Object.defineProperty(second, "arrayBuffer", { value: secondRead });

    render(
      <Composer
        scopeKey="runtime-a:tab:claude-a"
        value=""
        disabled={false}
        pending={false}
        supportsAttachments
        onChange={() => {}}
        onSubmit={async () => {}}
      />,
    );
    await user.upload(screen.getByLabelText(/attach image/i), [first, second]);

    expect((await screen.findByRole("alert")).textContent).toMatch(
      /5 MiB or less in total/i,
    );
    expect(firstRead).not.toHaveBeenCalled();
    expect(secondRead).not.toHaveBeenCalled();
  });
});
