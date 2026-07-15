import { ImagePlus, Send, X } from "lucide-react";
import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ClipboardEvent,
  type FormEvent,
  type KeyboardEvent,
} from "react";

import type { ComposerAttachment, WebAiKind } from "../api/types";
import type { ReturnBehavior } from "../settings/inputPreference";
import {
  buildImagePastePayload,
  inspectClipboardImageItems,
  WEB_PASTE_IMAGE_MAX_BYTES,
} from "../components/imagePaste";
import {
  filterCommandCatalog,
  replaceLeadingSlashToken,
} from "./commands/commandCatalog";
import { SlashCommandSheet } from "./commands/SlashCommandSheet";
import type { SlashCommand } from "./commands/types";
import { useSlashCommandCatalog } from "./commands/useSlashCommandCatalog";

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
  provider?: WebAiKind;
  catalogSessionKey?: string;
  returnBehavior?: ReturnBehavior;
  placeholder?: string;
  note?: string | null;
  onChange(value: string): void;
  onSubmit(text: string, attachments: ComposerAttachment[]): Promise<unknown>;
  onFocus?(): void;
  onSafetyStateChange?(state: {
    selectedAttachments: number;
    attachmentLoads: number;
  }): void;
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
  provider,
  catalogSessionKey = "",
  returnBehavior = "newline",
  placeholder = "Message",
  note = null,
  onChange,
  onSubmit,
  onFocus,
  onSafetyStateChange,
}: ComposerProps) {
  const [localValue, setLocalValue] = useState(value);
  const [attachments, setAttachments] = useState<PendingAttachment[]>([]);
  const [submitting, setSubmitting] = useState(false);
  const [readingAttachments, setReadingAttachments] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [activeCommandIndex, setActiveCommandIndex] = useState(0);
  const [dismissedCommandDraft, setDismissedCommandDraft] = useState<string | null>(null);
  const [selectedCommand, setSelectedCommand] = useState<SlashCommand | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const scopeRef = useRef(scopeKey);
  const scopeGenerationRef = useRef(0);
  const attachmentsRef = useRef<PendingAttachment[]>([]);
  const attachmentReadPendingRef = useRef(false);
  const onSafetyStateChangeRef = useRef(onSafetyStateChange);
  onSafetyStateChangeRef.current = onSafetyStateChange;
  const busy = pending || submitting || readingAttachments;
  const canSend = !disabled && !busy && (localValue.trim().length > 0 || attachments.length > 0);
  const slashEligible = Boolean(
    provider && localValue.startsWith("/") && !/\s/.test(localValue),
  );
  const catalog = useSlashCommandCatalog({
    scopeKey,
    sessionKey: catalogSessionKey,
    provider: provider ?? "claude",
    enabled: slashEligible,
  });
  const commandMatches = useMemo(
    () => (provider ? filterCommandCatalog(catalog.commands, localValue) : []),
    [catalog.commands, localValue, provider],
  );
  const commandSheetOpen = slashEligible && dismissedCommandDraft !== localValue;
  const publishSafety = (
    nextAttachments = attachmentsRef.current,
    loading = attachmentReadPendingRef.current,
  ) => {
    onSafetyStateChangeRef.current?.({
      selectedAttachments: nextAttachments.length,
      attachmentLoads: loading ? 1 : 0,
    });
  };

  useEffect(
    () => () => {
      scopeGenerationRef.current += 1;
      attachmentReadPendingRef.current = false;
      attachmentsRef.current = [];
      onSafetyStateChangeRef.current?.({
        selectedAttachments: 0,
        attachmentLoads: 0,
      });
    },
    [],
  );

  useLayoutEffect(() => {
    if (scopeRef.current === scopeKey) return;
    scopeRef.current = scopeKey;
    scopeGenerationRef.current += 1;
    attachmentReadPendingRef.current = false;
    attachmentsRef.current = [];
    publishSafety([], false);
    setLocalValue(value);
    setAttachments([]);
    setSubmitting(false);
    setReadingAttachments(false);
    setError(null);
    setActiveCommandIndex(0);
    setDismissedCommandDraft(null);
    setSelectedCommand(null);
  }, [scopeKey, value]);

  useEffect(() => setLocalValue(value), [value]);

  useEffect(() => {
    setActiveCommandIndex((current) =>
      commandMatches.length === 0 ? 0 : Math.min(current, commandMatches.length - 1),
    );
  }, [commandMatches.length]);

  useLayoutEffect(() => {
    const textarea = textareaRef.current;
    if (!textarea) return;
    textarea.style.height = "0px";
    textarea.style.height = `${Math.min(textarea.scrollHeight, 132)}px`;
  }, [localValue]);

  const updateValue = (next: string, keepSelectedCommand = false) => {
    setLocalValue(next);
    if (!keepSelectedCommand) {
      const token = next.split(/\s/, 1)[0];
      if (selectedCommand?.name !== token) setSelectedCommand(null);
    }
    onChange(next);
  };

  const acceptCommand = (command: SlashCommand) => {
    const next = replaceLeadingSlashToken(localValue, command.name);
    setSelectedCommand(command);
    setDismissedCommandDraft(next);
    setActiveCommandIndex(0);
    updateValue(next, true);
    textareaRef.current?.focus();
  };

  const applySuggestion = (command: SlashCommand, value: string) => {
    const next = `${command.name} ${value}`;
    setDismissedCommandDraft(next);
    updateValue(next, true);
    textareaRef.current?.focus();
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
    publishSafety(current, true);
    setReadingAttachments(true);
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
      publishSafety(next, true);
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
        publishSafety(attachmentsRef.current, false);
        setReadingAttachments(false);
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
      publishSafety([], false);
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
    if (commandSheetOpen) {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        if (commandMatches.length > 0) {
          const direction = event.key === "ArrowDown" ? 1 : -1;
          setActiveCommandIndex((current) =>
            (current + direction + commandMatches.length) % commandMatches.length,
          );
        }
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        setDismissedCommandDraft(localValue);
        return;
      }
      if (
        event.key === "Enter" &&
        !event.shiftKey &&
        !event.metaKey &&
        !event.ctrlKey &&
        !event.nativeEvent.isComposing
      ) {
        const match = commandMatches[activeCommandIndex];
        if (match) {
          event.preventDefault();
          acceptCommand(match.command);
          return;
        }
      }
    }
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
                  publishSafety(next, attachmentReadPendingRef.current);
                  setAttachments(next);
                }}
              >
                <X size={14} aria-hidden="true" />
              </button>
            </span>
          ))}
        </div>
      )}
      {commandSheetOpen && provider && (
        <SlashCommandSheet
          provider={provider}
          matches={commandMatches}
          activeIndex={activeCommandIndex}
          loading={catalog.loading}
          onActiveIndexChange={setActiveCommandIndex}
          onSelect={acceptCommand}
        />
      )}
      {selectedCommand && selectedCommand.suggestions.length > 0 && (
        <div className="dm-slash-command-suggestions" aria-label={`${selectedCommand.name} options`}>
          <span>{selectedCommand.argumentHint}</span>
          {selectedCommand.suggestions.map((suggestion) => (
            <button
              key={suggestion.value}
              type="button"
              aria-label={`Use ${suggestion.label}`}
              onClick={() => applySuggestion(selectedCommand, suggestion.value)}
            >
              {suggestion.label}
            </button>
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
