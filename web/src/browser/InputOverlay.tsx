import type { BrowserProjection, BrowserRemoteInput } from "./model";
import { mapProjectedInput } from "./model";

export function InputOverlay({
  projection,
  onInput,
}: {
  projection: BrowserProjection;
  onInput: (input: BrowserRemoteInput) => void;
}) {
  const disabled = projection.interactionMode !== "interact";

  return (
    <button
      type="button"
      className="dm-browser-input-overlay"
      aria-label="Projected page input overlay"
      disabled={disabled}
      onClick={(event) => {
        const bounds = event.currentTarget.getBoundingClientRect();
        const input: BrowserRemoteInput = {
          frameId: projection.frameId,
          generation: projection.generation,
          boundsEpoch: projection.boundsEpoch,
          focusEpoch: projection.focusEpoch,
          kind: "pointer",
          x: Math.round(event.clientX - bounds.left),
          y: Math.round(event.clientY - bounds.top),
          contentWidth: Math.max(1, Math.round(bounds.width)),
          contentHeight: Math.max(1, Math.round(bounds.height)),
          scale: 96,
        };
        const mapped = mapProjectedInput(projection, input);
        if (typeof mapped === "string") {
          return;
        }
        onInput({ ...input, x: mapped.x, y: mapped.y });
      }}
    />
  );
}
