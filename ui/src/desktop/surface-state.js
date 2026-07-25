/**
 * Reconciles native surface existence and metadata without overwriting the
 * persistent window frame owned by the React desktop.
 *
 * @param {Array<object>} current - Current z-ordered desktop surfaces.
 * @param {Array<object>} native - Latest native surface registry snapshot.
 * @returns {Array<object>} Reconciled surfaces in the existing z-order.
 */
export function reconcileSurfaces(current, native) {
  const byId = new Map(native.map((surface) => [surface.id, surface]));
  const kept = current
    .filter((surface) => byId.has(surface.id))
    .map((surface) => ({ ...byId.get(surface.id), bounds: surface.bounds }));
  const keptIds = new Set(kept.map((surface) => surface.id));
  return [...kept, ...native.filter((surface) => !keptIds.has(surface.id))];
}

/**
 * Applies the compositor's completed native move without changing window size.
 *
 * @param {Array<object>} surfaces - Current desktop surfaces.
 * @param {number} id - Moved surface identity.
 * @param {number} x - Final logical left edge.
 * @param {number} y - Final logical top edge.
 * @returns {Array<object>} Surfaces with the matching frame position updated.
 */
export function applySurfaceMove(surfaces, id, x, y) {
  return surfaces.map((surface) => (
    surface.id === id
      ? { ...surface, bounds: { ...surface.bounds, x, y } }
      : surface
  ));
}
