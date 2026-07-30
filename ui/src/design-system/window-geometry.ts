/** Grip that owns a resize drag. */
export type ResizeEdge = "n" | "s" | "e" | "w" | "ne" | "nw" | "se" | "sw";

/** A window frame rect in logical desktop pixels. */
export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/**
 * Converts a logical desktop frame into standard CSS positioning properties.
 *
 * @param rect - Frame measured in logical desktop pixels.
 * @returns Inline positioning properties accepted by a positioned HTML element.
 */
export function frameStyle(rect: Rect) {
  return {
    left: rect.x,
    top: rect.y,
    width: rect.width,
    height: rect.height,
  };
}

/** Pointer and frame captured when a resize grip is pressed. */
export interface ResizeOrigin extends Rect {
  startX: number;
  startY: number;
}

/** Candidate resize frame plus the metadata identifying its fixed far edge. */
export interface ResizeCandidate extends Rect {
  anchorRight: boolean;
  anchorBottom: boolean;
  right: number;
  bottom: number;
}

/**
 * Projects a captured window frame through one standard resize grip.
 */
export function projectResize(
  edge: ResizeEdge,
  origin: ResizeOrigin,
  point: { x: number; y: number },
): ResizeCandidate {
  const dx = point.x - origin.startX;
  const dy = point.y - origin.startY;
  let { x, y, width, height } = origin;
  if (edge.includes("e")) width = origin.width + dx;
  if (edge.includes("s")) height = origin.height + dy;
  if (edge.includes("w")) { x = origin.x + dx; width = origin.width - dx; }
  if (edge.includes("n")) { y = origin.y + dy; height = origin.height - dy; }
  return {
    x,
    y,
    width,
    height,
    anchorRight: edge.includes("w"),
    anchorBottom: edge.includes("n"),
    right: origin.x + origin.width,
    bottom: origin.y + origin.height,
  };
}

/**
 * Constrains a resize candidate to the desktop work area while keeping the
 * opposite edge fixed for north/west drags, matching native desktop resizing.
 */
export function constrainResize(
  rect: ResizeCandidate,
  workArea: Rect,
  minWidth: number,
  minHeight: number,
): Rect {
  let { x, y, width, height } = rect;
  const { anchorRight, anchorBottom, right, bottom } = rect;
  if (width < minWidth) { width = minWidth; if (anchorRight) x = right - minWidth; }
  if (height < minHeight) { height = minHeight; if (anchorBottom) y = bottom - minHeight; }

  // A north/west drag that crosses the work-area origin must shorten the frame
  // back to its fixed opposite edge. Clamping only x/y would move that edge,
  // making the window jump and grow unlike the native desktop interaction.
  if (x < workArea.x) {
    if (anchorRight) width = right - workArea.x;
    x = workArea.x;
  }
  if (y < workArea.y) {
    if (anchorBottom) height = bottom - workArea.y;
    y = workArea.y;
  }

  const workRight = workArea.x + workArea.width;
  const workBottom = workArea.y + workArea.height;
  width = Math.max(minWidth, Math.min(width, workRight - x));
  height = Math.max(minHeight, Math.min(height, workBottom - y));
  return { x, y, width, height };
}
