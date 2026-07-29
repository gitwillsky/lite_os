/**
 * Window system menu (titlebar right-click / taskbar button right-click).
 *
 * Pure item model so the desktop, the taskbar and the window frame share one
 * enable/disable truth, and so the rules stay node-testable without a host.
 * "Move" and "Size" exist in XP but need keyboard-driven window movement,
 * which is out of scope this round: they render disabled instead of being
 * dropped so the menu keeps the XP six-row shape.
 */

export interface SystemMenuState {
  minimized: boolean;
  maximized: boolean;
}

export interface SystemMenuItem {
  id: string;
  label: string;
  disabled: boolean;
  separator?: boolean;
}

export function systemMenuItems(state: SystemMenuState): SystemMenuItem[] {
  return [
    { id: "restore", label: "Restore", disabled: !state.minimized && !state.maximized },
    { id: "move", label: "Move", disabled: true },
    { id: "size", label: "Size", disabled: true },
    { id: "minimize", label: "Minimize", disabled: state.minimized },
    { id: "maximize", label: "Maximize", disabled: state.maximized },
    { id: "separator", label: "", disabled: true, separator: true },
    { id: "close", label: "Close", disabled: false },
  ];
}
