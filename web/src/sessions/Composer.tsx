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
import {
  buildImagePastePayload,
  inspectClipboardImageItems,
  WEB_PASTE_IMAGE_MAX_BYTES,
} from "../components/imagePaste";

const MAX_ATTACHMENTS = 4;
const MAX_ATTACHMENT_TOTAL_BYTES = 10 * 1024 * 1024;

interface PendingAttachment extends ComposerAttachment {
  id: string;
  byteLength: number;
}

export interface ComposerProps {
  value: string;
  disabled: boolean;
  pending: boolean;
  supportsAttachments: boolean;
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
  value,
  disabled,
  pending,
  supportsAttachments,
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
  const busy = pending || submitting;
  const canSend = !disabled && !busy && (localValue.trim().length > 0 || attachments.length > 0);

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
    try {
      const additions: PendingAttachment[] = [];
      for (const file of files) {
        if (file.size > WEB_PASTE_IMAGE_MAX_BYTES) {
          throw new Error(`${file.name || "Image"} is larger than 5 MiB.`);
        }
        const payload = await buildImagePastePayload(file);
        additions.push({
          id: attachmentId(),
          mimeType: payload.mimeType,
          fileName: payload.fileName ?? null,
          dataBase64: payload.dataBase64,
          byteLength: file.size,
        });
      }
      setAttachments((current) => {
        const next = [...current, ...additions];
        if (next.length > MAX_ATTACHMENTS) {
          setError("Attach no more than four images.");
          return current;
        }
        if (next.reduce((total, attachment) => total + attachment.byteLength, 0) > MAX_ATTACHMENT_TOTAL_BYTES) {
          setError("Attachments must be 10 MiB or less in total.");
          return current;
        }
        return next;
      });
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : "That image could not be attached.");
    }
  };

  const submit = async () => {
    if (!canSend) return;
    setSubmitting(true);
    setError(null);
    try {
      await onSubmit(
        localValue,
        attachments.map(({ mimeType, fileName, dataBase64 }) => ({ mimeType, fileName, dataBase64 })),
      );
      setAttachments([]);
      updateValue("");
    } catch (caught) {
      setError(caught instanceof Error ? caught.message : "Your message could not be sent yet.");
    } finally {
      setSubmitting(false);
    }
  };

  const onFormSubmit = (event: FormEvent) => {
    event.preventDefault();
    void submit();
  };

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey) && !event.nativeEvent.isComposing) {
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
              <button type="button" aria-label={`Remove ${attachment.fileName ?? "image"}`} onClick={() => setAttachments((current) => current.filter((item) => item.id !== attachment.id))}>
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
      <p className="dm-composer-hint">⌘↵ to send · Shift↵ for a new line</p>
    </form>
  );
}
