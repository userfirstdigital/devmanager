import { ImagePlus, Send, X } from "lucide-react";
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ClipboardEvent,
  type FormEvent,
  type KeyboardEvent,
} from "react";

import type { ComposerAttachment } from "../api/types";
import type { ReturnBehavior } from "../settings/inputPreference";
import {
  buildImagePastePayload,
  inspectClipboardImageItems,
  WEB_PASTE_IMAGE_MAX_BYTES,
} from "../components/imagePaste";

const MAX_ATTACHMENTS = 4;
// The socket keeps one shared 8 MiB encoded outbound budget. Base64 expands
// image bytes by roughly one third, so a 5 MiB raw batch leaves room for the
// message, filenames, JSON envelope, and other acknowledged work.
const MAX_ATTACHMENT_TOTAL_BYTES = 5 * 1024 * 1024;
const SUPPORTED_ATTACHMENT_TYPES = new Set(["image/png", "image/jpeg"]);

interface PendingAttachment extends ComposerAttachment {
  id: string;
  byteLength: number;
}

export interface ComposerProps {
  /** Runtime + stable-session identity. Local attachment state never crosses it. */
  scopeKey: string;
  value: string;
  disabled: boolean;
  pending: boolean;
  supportsAttachments: boolean;
  returnBehavior?: ReturnBehavior;
  placeholder?: string;
  note?: string | null;
  onChange(value: string): void;
  onSubmit(text: string, attachments: ComposerAttachment[]): Promise<unknown>;
  onFocus?(): void;
}

function attachmentId(): string {
  return globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random()}`;
}

export function Composer({
  scopeKey,
  value,
  disabled,
  pending,
  supportsAttachments,
  returnBehavior = "newline",
  placeholder = "Message",
  note = null,
  onChange,
  onSubmit,
  onFocus,
}: ComposerProps) {
  const [localValue, setLocalValue] = useState(value);
  const [attachments, setAttachments] = useState<PendingAttachment[]>([]);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const scopeRef = useRef(scopeKey);
  const scopeGenerationRef = useRef(0);
  const attachmentsRef = useRef<PendingAttachment[]>([]);
  const attachmentReadPendingRef = useRef(false);
  const busy = pending || submitting;
  const canSend = !disabled && !busy && (localValue.trim().length > 0 || attachments.length > 0);

  useLayoutEffect(() => {
    if (scopeRef.current === scopeKey) return;
    scopeRef.current = scopeKey;
    scopeGenerationRef.current += 1;
    attachmentReadPendingRef.current = false;
    attachmentsRef.current = [];
    setLocalValue(value);
    setAttachments([]);
    setSubmitting(false);
    setError(null);
  }, [scopeKey, value]);

  useEffect(() => setLocalValue(value), [value]);

  useLayoutEffect(() => {
    const textarea = textareaRef.current;
    if (!textarea) return;
    textarea.style.height = "0px";
    textarea.style.height = `${Math.min(textarea.scrollHeight, 132)}px`;
  }, [localValue]);

  const updateValue = (next: string) => {
    setLocalValue(next);
    onChange(next);
  };

  const addFiles = async (files: File[]) => {
    if (!supportsAttachments || files.length === 0) return;
    setError(null);
    if (attachmentReadPendingRef.current) {
      setError("Wait for the current images to finish attaching.");
      return;
    }
    const current = attachmentsRef.current;
    if (current.length + files.length > MAX_ATTACHMENTS) {
      setError("Attach no more than four images.");
      return;
    }
    const unsupported = files.find(
      (file) => !SUPPORTED_ATTACHMENT_TYPES.has(file.type),
    );
    if (unsupported) {
      setError("Only PNG and JPEG images are supported.");
      return;
    }
    const oversized = files.find((file) => file.size > WEB_PASTE_IMAGE_MAX_BYTES);
    if (oversized) {
      setError(`${oversized.name || "Image"} is larger than 5 MiB.`);
      return;
    }
    const selectedBytes = files.reduce((total, file) => total + file.size, 0);
    const currentBytes = current.reduce(
      (total, attachment) => total + attachment.byteLength,
      0,
    );
    if (currentBytes + selectedBytes > MAX_ATTACHMENT_TOTAL_BYTES) {
      setError("Attachments must be 5 MiB or less in total.");
      return;
    }

    const operationScope = scopeRef.current;
    const operationGeneration = scopeGenerationRef.current;
    attachmentReadPendingRef.current = true;
    try {
      const additions: PendingAttachment[] = [];
      for (const file of files) {
        const payload = await buildImagePastePayload(file);
        if (
          scopeRef.current !== operationScope ||
          scopeGenerationRef.current !== operationGeneration
        ) {
          return;
        }
        additions.push({
          id: attachmentId(),
          mimeType: payload.mimeType,
          fileName: payload.fileName ?? null,
          dataBase64: payload.dataBase64,
          byteLength: file.size,
        });
      }
      const next = [...current, ...additions];
      attachmentsRef.current = next;
      setAttachments(next);
    } catch (caught) {
      if (
        scopeRef.current === operationScope &&
        scopeGenerationRef.current === operationGeneration
      ) {
        setError(caught instanceof Error ? caught.message : "That image could not be attached.");
      }
    } finally {
      if (
        scopeRef.current === operationScope &&
        scopeGenerationRef.current === operationGeneration
      ) {
        attachmentReadPendingRef.current = false;
      }
    }
  };

  const submit = async () => {
    if (!canSend) return;
    const operationScope = scopeRef.current;
    const operationGeneration = scopeGenerationRef.current;
    setSubmitting(true);
    setError(null);
    try {
      await onSubmit(
        localValue,
        attachments.map(({ mimeType, fileName, dataBase64 }) => ({ mimeType, fileName, dataBase64 })),
      );
      if (
        scopeRef.current !== operationScope ||
        scopeGenerationRef.current !== operationGeneration
      ) {
        return;
      }
      attachmentsRef.current = [];
      setAttachments([]);
      updateValue("");
    } catch (caught) {
      if (
        scopeRef.current === operationScope &&
        scopeGenerationRef.current === operationGeneration
      ) {
        setError(caught instanceof Error ? caught.message : "Your message could not be sent yet.");
      }
    } finally {
      if (
        scopeRef.current === operationScope &&
        scopeGenerationRef.current === operationGeneration
      ) {
        setSubmitting(false);
      }
    }
  };

  const onFormSubmit = (event: FormEvent) => {
    event.preventDefault();
    void submit();
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    const shouldSend =
      event.key === "Enter" &&
      !event.shiftKey &&
      !event.nativeEvent.isComposing &&
      (returnBehavior === "send" || event.metaKey || event.ctrlKey);
    if (shouldSend) {
      event.preventDefault();
      void submit();
    }
  };

  const onPaste = (event: ClipboardEvent<HTMLTextAreaElement>) => {
    if (!supportsAttachments) return;
    const inspected = inspectClipboardImageItems(event.clipboardData?.items);
    if (inspected.kind === "supported") {
      event.preventDefault();
      void addFiles([inspected.file]);
    } else if (inspected.kind === "unsupported") {
      setError("Only PNG and JPEG images are supported.");
    }
  };

  return (
    <form className="dm-composer" onSubmit={onFormSubmit} aria-label="Session composer">
      {(disabled || note) && (
        <p className="dm-composer-connection" role="status">
          {disabled ? "Reconnecting… your draft is safe." : note}
        </p>
      )}
      {attachments.length > 0 && (
        <div className="dm-attachment-list" aria-label="Attached images">
          {attachments.map((attachment) => (
            <span className="dm-attachment-chip" key={attachment.id}>
              {attachment.fileName ?? "Image"}
              <button
                type="button"
                aria-label={`Remove ${attachment.fileName ?? "image"}`}
                onClick={() => {
                  const next = attachmentsRef.current.filter(
                    (item) => item.id !== attachment.id,
                  );
                  attachmentsRef.current = next;
                  setAttachments(next);
                }}
              >
                <X size={14} aria-hidden="true" />
              </button>
            </span>
          ))}
        </div>
      )}
      <div className="dm-composer-bar">
        {supportsAttachments && (
          <label className="dm-composer-icon-button">
            <ImagePlus size={21} aria-hidden="true" />
            <input
              type="file"
              aria-label="Attach image"
              accept="image/png,image/jpeg"
              capture="environment"
              multiple
              disabled={disabled || busy}
              onChange={(event) => {
                void addFiles(Array.from(event.currentTarget.files ?? []));
                event.currentTarget.value = "";
              }}
            />
          </label>
        )}
        <textarea
          ref={textareaRef}
          aria-label="Message"
          autoCapitalize="sentences"
          autoCorrect="on"
          enterKeyHint="send"
          inputMode="text"
          rows={1}
          placeholder={placeholder}
          value={localValue}
          disabled={disabled}
          onChange={(event) => updateValue(event.currentTarget.value)}
          onFocus={onFocus}
          onKeyDown={onKeyDown}
          onPaste={onPaste}
        />
        <button type="submit" className="dm-composer-send" aria-label="Send message" disabled={!canSend}>
          <Send size={19} aria-hidden="true" />
        </button>
      </div>
      {error && <p className="dm-composer-error" role="alert">{error}</p>}
      <p className="dm-composer-hint">
        {returnBehavior === "send"
          ? "Return to send · Shift↵ for a new line"
          : "⌘↵ to send · Return for a new line"}
      </p>
    </form>
  );
}
