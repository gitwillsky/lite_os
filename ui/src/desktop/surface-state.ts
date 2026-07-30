import type { Rect } from "../design-system/window-geometry.ts";

/**
 * Reconciles native surface existence and metadata without overwriting the
 * persistent window frame owned by the React desktop.
 *
 * @param current - Current z-ordered desktop surfaces.
 * @param native - Latest native surface registry snapshot.
 * @returns Reconciled surfaces in the existing z-order.
 */
export function reconcileSurfaces(
  current: LiteSurface[],
  native: LiteSurface[],
): LiteSurface[] {
  const byId = new Map(native.map((surface) => [surface.id, surface]));
  const kept = current
    .filter((surface) => byId.has(surface.id))
    .map((surface) => ({ ...byId.get(surface.id)!, bounds: surface.bounds }));
  const keptIds = new Set(kept.map((surface) => surface.id));
  return [...kept, ...native.filter((surface) => !keptIds.has(surface.id))];
}

/**
 * Fits one persistent window frame inside the current logical work area.
 *
 * @param bounds - Existing React-owned window frame.
 * @param workArea - Current viewport's usable desktop rectangle.
 * @returns A frame no larger than, and fully contained by, the work area.
 */
export function fitSurfaceFrame(bounds: Rect, workArea: Rect): Rect {
  const width = Math.min(bounds.width, workArea.width);
  const height = Math.min(bounds.height, workArea.height);
  return {
    x: Math.max(workArea.x, Math.min(workArea.x + workArea.width - width, bounds.x)),
    y: Math.max(workArea.y, Math.min(workArea.y + workArea.height - height, bounds.y)),
    width,
    height,
  };
}

/**
 * Applies the compositor's completed native move without changing window size.
 *
 * @param surfaces - Current desktop surfaces.
 * @param id - Moved surface identity.
 * @param x - Final logical left edge.
 * @param y - Final logical top edge.
 * @returns Surfaces with the matching frame position updated.
 */
export function applySurfaceMove(
  surfaces: LiteSurface[],
  id: number,
  x: number,
  y: number,
): LiteSurface[] {
  return surfaces.map((surface) => (
    surface.id === id
      ? { ...surface, bounds: { ...surface.bounds, x, y } }
      : surface
  ));
}
