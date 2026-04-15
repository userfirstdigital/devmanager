export const DESKTOP_TERMINAL_FONT_SIZE = 13;
export const MOBILE_TERMINAL_MIN_FONT_SIZE = 8;
const MOBILE_TERMINAL_BREAKPOINT_PX = 768;

export interface ResponsiveTerminalLayoutInput {
  containerWidth: number;
  measuredTerminalWidth: number;
  currentFontSize: number;
  baseFontSize?: number;
  minFontSize?: number;
}

export interface ResponsiveTerminalLayout {
  fontSize: number;
  allowHorizontalPan: boolean;
}

export function computeResponsiveTerminalLayout(
  input: ResponsiveTerminalLayoutInput,
): ResponsiveTerminalLayout {
  const baseFontSize = input.baseFontSize ?? DESKTOP_TERMINAL_FONT_SIZE;
  const minFontSize = input.minFontSize ?? MOBILE_TERMINAL_MIN_FONT_SIZE;
  const { containerWidth, measuredTerminalWidth, currentFontSize } = input;

  if (containerWidth <= 0 || measuredTerminalWidth <= 0 || currentFontSize <= 0) {
    return {
      fontSize: baseFontSize,
      allowHorizontalPan: false,
    };
  }

  const estimatedBaseWidth =
    measuredTerminalWidth * (baseFontSize / currentFontSize);

  if (
    containerWidth >= MOBILE_TERMINAL_BREAKPOINT_PX ||
    estimatedBaseWidth <= containerWidth
  ) {
    return {
      fontSize: baseFontSize,
      allowHorizontalPan: false,
    };
  }

  const fittedFontSize = Math.floor(
    (baseFontSize * containerWidth) / estimatedBaseWidth,
  );
  const fontSize = Math.max(
    minFontSize,
    Math.min(baseFontSize, fittedFontSize),
  );
  const estimatedWidth = estimatedBaseWidth * (fontSize / baseFontSize);

  return {
    fontSize,
    allowHorizontalPan: estimatedWidth > containerWidth + 0.5,
  };
}
