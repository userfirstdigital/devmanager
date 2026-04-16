import { describe, expect, it } from "vitest";

import {
  buildImagePastePayload,
  inspectClipboardImageItems,
  WEB_PASTE_IMAGE_MAX_BYTES,
} from "./imagePaste";

describe("imagePaste helpers", () => {
  it("picks the first supported clipboard image item", () => {
    const pngFile = new File([new Uint8Array([0x89, 0x50, 0x4e, 0x47])], "shot.png", {
      type: "image/png",
    });
    const items = [
      {
        type: "text/plain",
        getAsFile: () => null,
      },
      {
        type: "image/png",
        getAsFile: () => pngFile,
      },
    ];

    expect(inspectClipboardImageItems(items)).toEqual({
      kind: "supported",
      file: pngFile,
      mimeType: "image/png",
    });
  });

  it("reports unsupported image clipboard types", () => {
    const gifFile = new File([new Uint8Array([0x47, 0x49, 0x46])], "shot.gif", {
      type: "image/gif",
    });
    const items = [
      {
        type: "image/gif",
        getAsFile: () => gifFile,
      },
    ];

    expect(inspectClipboardImageItems(items)).toEqual({
      kind: "unsupported",
      mimeType: "image/gif",
    });
  });

  it("encodes clipboard files as web image paste payloads", async () => {
    const file = new File([new Uint8Array([0x01, 0x02, 0x03])], "clip.png", {
      type: "image/png",
    });

    await expect(buildImagePastePayload(file)).resolves.toEqual({
      mimeType: "image/png",
      fileName: "clip.png",
      dataBase64: "AQID",
    });
  });

  it("rejects clipboard images above the frontend size limit", async () => {
    const file = new File([new Uint8Array(WEB_PASTE_IMAGE_MAX_BYTES + 1)], "large.png", {
      type: "image/png",
    });

    await expect(buildImagePastePayload(file)).rejects.toThrow(
      "Pasted image is too large",
    );
  });
});
