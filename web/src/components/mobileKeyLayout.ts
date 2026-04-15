export interface MobileKeyDefinition {
  label: string;
  payload: string;
}

const COMPACT_MOBILE_KEY_LABELS = [
  "Esc",
  "Tab",
  "Ctrl",
  "C",
  "D",
  "Z",
  "L",
  "Enter",
  "Up",
  "Down",
  "Right",
];

const ESSENTIAL_MOBILE_KEY_LABELS = [
  "Esc",
  "Tab",
  "Ctrl",
  "C",
  "D",
  "Enter",
  "Up",
  "Down",
];

export const MOBILE_KEY_LAYOUT: MobileKeyDefinition[] = [
  { label: "Esc", payload: "\u001b" },
  { label: "Tab", payload: "\t" },
  { label: "Ctrl", payload: "" },
  { label: "C", payload: "c" },
  { label: "D", payload: "d" },
  { label: "Z", payload: "z" },
  { label: "L", payload: "l" },
  { label: "Up", payload: "\u001bOA" },
  { label: "Down", payload: "\u001bOB" },
  { label: "Enter", payload: "\r" },
  { label: "Right", payload: "\u001bOC" },
  { label: "Pipe", payload: "|" },
  { label: "Slash", payload: "/" },
  { label: "Tilde", payload: "~" },
];

function keysFromLabels(labels: string[]): MobileKeyDefinition[] {
  const keyByLabel = new Map(
    MOBILE_KEY_LAYOUT.map((key) => [key.label, key] as const),
  );
  return labels
    .map((label) => keyByLabel.get(label))
    .filter((key): key is MobileKeyDefinition => key != null);
}

export function pickMobileKeysForWidth(
  containerWidth: number,
): MobileKeyDefinition[] {
  if (containerWidth > 0 && containerWidth < 400) {
    return keysFromLabels(ESSENTIAL_MOBILE_KEY_LABELS);
  }

  if (containerWidth > 0 && containerWidth < 500) {
    return keysFromLabels(COMPACT_MOBILE_KEY_LABELS);
  }

  return MOBILE_KEY_LAYOUT;
}
