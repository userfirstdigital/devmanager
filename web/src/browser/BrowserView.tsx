import { InputOverlay } from "./InputOverlay";
import type { BrowserProjection, BrowserRemoteInput } from "./model";
import { pixelsAreLocalDom } from "./model";

export function BrowserView({
  projection,
  phoneWidth = 390,
  onInput,
}: {
  projection: BrowserProjection;
  phoneWidth?: number;
  onInput?: (input: BrowserRemoteInput) => void;
}) {
  const selected =
    projection.tabs.find((tab) => tab.tabId === projection.selectedTabId) ??
    projection.tabs[0];

  return (
    <section
      className="dm-browser-view"
      data-local-dom={pixelsAreLocalDom() ? "true" : "false"}
      style={{ width: "100%", maxWidth: phoneWidth, minHeight: "100dvh" }}
      aria-label="Projected task browser"
    >
      <header className="dm-browser-chrome">
        <div className="dm-browser-tabs" role="tablist">
          {projection.tabs.map((tab) => (
            <span
              key={tab.tabId}
              role="tab"
              aria-selected={tab.tabId === selected?.tabId}
            >
              {tab.title || tab.url}
            </span>
          ))}
        </div>
        <p>{selected?.url ?? ""}</p>
        <p>
          {selected?.loading
            ? "Loading"
            : selected?.error
              ? selected.error
              : selected?.security}
        </p>
        {projection.progress && <p>{projection.progress}</p>}
        <p>Mode: {projection.interactionMode}</p>
      </header>
      <div className="dm-browser-frame" role="img" aria-label="Projected screenshot, not a local document">
        {projection.frameSrc ? (
          <img src={projection.frameSrc} alt="Projected browser frame" />
        ) : (
          <div>No projected frame</div>
        )}
        {onInput && (
          <InputOverlay projection={projection} onInput={onInput} />
        )}
      </div>
    </section>
  );
}
