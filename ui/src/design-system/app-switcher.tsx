import React from "react";
import type { SwitcherState } from "../desktop/app-switcher.ts";
import { selectedCandidate } from "../desktop/app-switcher.ts";

// The switcher is one `position:fixed` overlay clip re-blitted above every
// foreign surface, so its rectangle must be exact — no percent or transform
// centering. The width follows the candidate count; the height is fixed.
// Viewport matches `#desktop` in desktop/style.css (1504x846 logical pixels).
const VIEWPORT = { width: 1504, height: 846 };
/** One icon cell (40px) plus the 4px row gap. */
const ICON_SLOT = 44;
/** 8px padding + 40px icons + 6px gap + 13px title + 8px padding + 4px border. */
const PANEL_HEIGHT = 79;
const MIN_WIDTH = 280;
const MAX_WIDTH = 1200;

/**
 * XP Alt+Tab panel: raised gray bevel, one row of app icons with a sunken
 * highlight on the selection, and the selected window's title below.
 *
 * @param {{state: import("../desktop/app-switcher.ts").SwitcherState}} props
 */
export function AppSwitcher({ state }: { state: SwitcherState }) {
  const iconsWidth = state.candidates.length * ICON_SLOT - 4;
  const width = Math.min(Math.max(iconsWidth + 16, MIN_WIDTH), MAX_WIDTH);
  const left = Math.floor((VIEWPORT.width - width) / 2);
  const top = Math.floor((VIEWPORT.height - PANEL_HEIGHT) / 2);
  return (
    <div className="app-switcher" style={{ left, top, width }}>
      <div className="app-switcher__icons">
        {state.candidates.map((candidate, index) => (
          <div
            key={candidate.id}
            className={index === state.selection ? "app-switcher__item app-switcher__item--selected" : "app-switcher__item"}
          >
            <img className="app-switcher__icon" src={candidate.icon}/>
          </div>
        ))}
      </div>
      <span className="app-switcher__title">{selectedCandidate(state).title}</span>
    </div>
  );
}
