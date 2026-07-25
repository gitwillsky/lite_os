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
