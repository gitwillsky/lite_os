/**
 * Pure state model for the XP Alt+Tab window switcher. The compositor key grab
 * delivers the whole chord sequence to the desktop; this module only decides
 * which window is selected, so the React layer stays a thin renderer.
 *
 * Candidates are kept in MRU order: index 0 is the current window (the end of
 * the z-ordered `open` list), index 1 the previous one. Opening selects index
 * 1, so a bare Alt+Tab tap — release Alt without another Tab — activates the
 * previous window (XP semantics). Minimized windows stay candidates; committing
 * one restores it through the normal activate path.
 */

/** One switcher row: the window metadata the panel renders. */
export interface SwitcherCandidate {
  id: number;
  title: string;
  icon: string;
}

/** Open switcher panel: MRU candidates plus the highlighted row. */
export interface SwitcherState {
  candidates: SwitcherCandidate[];
  selection: number;
}

/**
 * Opens the switcher over the current z-ordered window list.
 *
 * @param open - Z-ordered open windows; the last entry is the current window.
 * @returns The initial state (selection on the previous window), or `null`
 *   when no window is open and no panel should appear.
 */
export function openSwitcher(open: SwitcherCandidate[]): SwitcherState | null {
  if (open.length === 0) return null;
  const candidates = open.slice().reverse();
  return { candidates, selection: candidates.length > 1 ? 1 : 0 };
}

/**
 * Advances the selection by one Tab press, wrapping at both ends.
 *
 * @param state - Current switcher state.
 * @param backward - `true` while Shift is held (Shift+Tab walks backwards).
 * @returns The state with the selection moved one row.
 */
export function cycle(state: SwitcherState, backward: boolean): SwitcherState {
  const count = state.candidates.length;
  const selection = (state.selection + (backward ? -1 : 1) + count) % count;
  return { ...state, selection };
}

/**
 * Rebuilds the candidates after the window set changed mid-grab (a window
 * opened or closed) and clamps the selection into the valid range.
 *
 * @param state - Current switcher state.
 * @param open - Latest z-ordered open windows.
 * @returns The clamped state, or `null` when no candidate survives — the
 *   caller closes the panel and the commit becomes a no-op.
 */
export function reconcileSwitcher(
  state: SwitcherState,
  open: SwitcherCandidate[],
): SwitcherState | null {
  if (open.length === 0) return null;
  const candidates = open.slice().reverse();
  return {
    candidates,
    selection: Math.min(state.selection, candidates.length - 1),
  };
}

/**
 * Resolves the window committed when Alt is released.
 *
 * @param state - Current switcher state.
 * @returns The selected candidate.
 */
export function selectedCandidate(state: SwitcherState): SwitcherCandidate {
  return state.candidates[state.selection];
}
