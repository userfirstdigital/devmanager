import type { WebImagePastePayload } from "../api/types";

export const WEB_PASTE_IMAGE_MAX_BYTES = 5 * 1024 * 1024;

const SUPPORTED_IMAGE_MIME_TYPES = new Set(["image/png", "image/jpeg"]);

type ClipboardLikeItem = {
  type: string;
  getAsFile?: () => File | null;
};

export type ClipboardImageInspection =
  | {
      kind: "supported";
      file: File;
      mimeType: "image/png" | "image/jpeg";
    }
  | {
      kind: "unsupported";
      mimeType: string;
    }
  | {
      kind: "none";
    };

export function isAiSessionKind(kind: unknown): kind is "Claude" | "Codex" {
  return kind === "Claude" || kind === "Codex";
}

export function inspectClipboardImageItems(
  items: ArrayLike<ClipboardLikeItem> | null | undefined,
): ClipboardImageInspection {
  if (!items) {
    return { kind: "none" };
  }

  let unsupportedMimeType: string | null = null;
  for (const item of Array.from(items)) {
    if (!item.type.startsWith("image/")) {
      continue;
    }
    if (SUPPORTED_IMAGE_MIME_TYPES.has(item.type)) {
      const file = item.getAsFile?.();
      if (file) {
        return {
          kind: "supported",
          file,
          mimeType: item.type as "image/png" | "image/jpeg",
        };
      }
    }
    unsupportedMimeType ??= item.type;
  }

  if (unsupportedMimeType) {
    return { kind: "unsupported", mimeType: unsupportedMimeType };
  }

  return { kind: "none" };
}

function defaultFileNameForMimeType(mimeType: "image/png" | "image/jpeg"): string {
  return mimeType === "image/jpeg" ? "clipboard-image.jpg" : "clipboard-image.png";
}

function bytesToBase64(bytes: Uint8Array): string {
  let binary = "";
  const chunkSize = 0x8000;
  for (let offset = 0; offset < bytes.length; offset += chunkSize) {
    const chunk = bytes.subarray(offset, offset + chunkSize);
    binary += String.fromCharCode(...chunk);
  }
  return btoa(binary);
}

export async function buildImagePastePayload(
  file: Blob,
): Promise<WebImagePastePayload> {
  if (!SUPPORTED_IMAGE_MIME_TYPES.has(file.type)) {
    throw new Error("Unsupported pasted image type. Try PNG or JPEG.");
  }
  if (file.size > WEB_PASTE_IMAGE_MAX_BYTES) {
    throw new Error("Pasted image is too large. Max size is 5 MiB.");
  }

  const mimeType = file.type as "image/png" | "image/jpeg";
  const fileName =
    file instanceof File && file.name
      ? file.name
      : defaultFileNameForMimeType(mimeType);
  const bytes = new Uint8Array(await file.arrayBuffer());

  return {
    mimeType,
    fileName,
    dataBase64: bytesToBase64(bytes),
  };
}
