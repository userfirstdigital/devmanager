import { describe, expect, it } from "vitest";

import {
  BoundedResponseError,
  readBoundedResponseText,
  withResponseAbort,
} from "./boundedResponse";

describe("boundedResponse", () => {
  it("races withResponseAbort independently of native cancel", async () => {
    const controller = new AbortController();
    const hanging = new Promise<string>(() => undefined);
    const pending = withResponseAbort(hanging, controller.signal, "deadline");
    controller.abort();
    await expect(pending).rejects.toBeInstanceOf(BoundedResponseError);
  });

  it("rejects an abort-resistant hanging reader under the deadline without accepting a partial body", async () => {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 30);
    const stream = new ReadableStream<Uint8Array>({
      start(controllerStream) {
        controllerStream.enqueue(new TextEncoder().encode('{"partial":'));
        // Never closes; cancel intentionally hangs.
      },
      cancel() {
        return new Promise(() => undefined);
      },
    });
    const response = new Response(stream, { status: 200 });
    await expect(
      readBoundedResponseText(response, 16_384, controller.signal, "rejected"),
    ).rejects.toBeInstanceOf(BoundedResponseError);
    clearTimeout(timer);
  });

  it("enforces the byte cap without awaiting a stuck cancel", async () => {
    const controller = new AbortController();
    const stream = new ReadableStream<Uint8Array>({
      pull(controllerStream) {
        controllerStream.enqueue(new Uint8Array(8).fill(65));
      },
      cancel() {
        return new Promise(() => undefined);
      },
    });
    const response = new Response(stream, { status: 200 });
    await expect(
      readBoundedResponseText(response, 16, controller.signal, "oversized"),
    ).rejects.toMatchObject({ message: "oversized" });
  });

  it("fail-closes when Response has no stream reader", async () => {
    const controller = new AbortController();
    await expect(
      readBoundedResponseText(
        { ok: true, body: null } as unknown as Response,
        64,
        controller.signal,
        "no-reader",
      ),
    ).rejects.toMatchObject({ message: "no-reader" });
  });
});
