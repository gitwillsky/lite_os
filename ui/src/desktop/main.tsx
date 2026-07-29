import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { apps, launch } from "lite:apps";
import { beginMove, close, configure, focus, move, surfaces, shutdown, setAccelerators } from "lite:desktop";
import { Window } from "../design-system/window.tsx";
import { Taskbar } from "../design-system/taskbar.tsx";
import { StartMenu } from "../design-system/start-menu.tsx";
import { ContextMenu } from "../design-system/context-menu.tsx";
import { AppSwitcher } from "../design-system/app-switcher.tsx";
import { systemMenuItems } from "../design-system/system-menu.ts";
import { PropertiesPopup } from "../design-system/properties-popup.tsx";
import { constrainResize } from "../design-system/window-geometry.ts";
import type { Rect, ResizeCandidate } from "../design-system/window-geometry.ts";
import { applySurfaceMove, reconcileSurfaces } from "./surface-state.ts";
import { cycle, openSwitcher, reconcileSwitcher, selectedCandidate } from "./app-switcher.ts";
import type { SwitcherState } from "./app-switcher.ts";

interface DesktopMenuItem {
  id: string;
  label: string;
  onSelect?: () => void;
  disabled?: boolean;
  separator?: boolean;
}

interface MenuState {
  x: number;
  y: number;
  items: DesktopMenuItem[];
}

const desktopIcons = [
  { id: "computer", label: "My Computer", icon: "assets/computer.png", app: "my-computer" },
  { id: "terminal", label: "Terminal", icon: "assets/terminal.png", app: "terminal" },
  { id: "music-player", label: "Music Player", icon: "assets/speaker.png", app: "music-player" },
  { id: "documents", label: "My Documents", icon: "assets/documents.png", app: "file-manager" },
  { id: "trash", label: "Recycle Bin", icon: "assets/trash.png" },
];

interface PropertiesState {
  x: number;
  y: number;
  title: string;
  rows: [string, string][];
}

// Linux evdev KEY_ESC. Escape dismisses open popups when the desktop is focused.
const KEY_ESC = 1;
// evdev codes for the desktop accelerator chords and their grab sequences.
const KEY_TAB = 15;
const KEY_LEFTALT = 56;
const KEY_F4 = 62;
const KEY_RIGHTALT = 100;
// display-proto modifier mask bits (compositor input.rs `update_modifier`):
// Shift = 1, Ctrl = 2, Alt = 4, Super = 8. Chord matching is exact, so the
// registered chords below list the full required mask.
const MOD_SHIFT = 1;
const MOD_CTRL = 2;
const MOD_ALT = 4;

// The taskbar-free area every maximized window covers; move clamps agree.
const WORK_AREA = { x: 0, y: 0, width: 1504, height: 816 };
// Minimum window frame size. Comfortably above the classic chrome insets (10
// wide, 32 tall) so the client area stays usable and
// `configure(w - 10, h - 32)` never underflows the u32 the host parses.
const MIN_W = 160;
const MIN_H = 120;
const clampX = (x: number, width: number) => Math.max(0, Math.min(WORK_AREA.width - width, x));
const clampY = (y: number, height: number) => Math.max(0, Math.min(WORK_AREA.height - height, y));

export default function Desktop() {
  const [open, setOpen] = useState(() => surfaces());
  // Live mirror of `open` so stable callbacks read current z-order without a
  // stale closure (see `minimizedRef`).
  const openRef = useRef(open);
  openRef.current = open;
  const [activeId, setActiveId] = useState<number>(() => open.at(-1)?.id ?? 0);
  // Live mirror for the key handler (Alt+F4 closes the active window).
  const activeIdRef = useRef(activeId);
  activeIdRef.current = activeId;
  // Open Alt+Tab panel; null while no grab is in progress.
  const [switcher, setSwitcher] = useState<SwitcherState | null>(null);
  // Live mirror so the key handler commits the latest selection even when the
  // Alt-up event lands before the render of the last Tab cycle.
  const switcherRef = useRef(switcher);
  switcherRef.current = switcher;
  const [minimized, setMinimized] = useState<Set<number>>(() => new Set());
  // Live mirror of `minimized` so the stable `[]`-deps subscribe callback and
  // callbacks can read the current set without a stale closure or a state
  // setter's side effects.
  const minimizedRef = useRef(minimized);
  minimizedRef.current = minimized;
  // id -> bounds saved when the window was maximized; restore reads them back.
  const [maximized, setMaximized] = useState<Map<number, LiteFrame>>(() => new Map());
  // id -> outline bounds shown during classic resize. Keeping this separate
  // from `open` prevents every pointer motion from configuring a new app
  // buffer; without it the gray window body and app pixels alternate onscreen.
  const [resizePreview, setResizePreview] = useState<Map<number, Rect>>(() => new Map());
  const resizePreviewRef = useRef(resizePreview);
  resizePreviewRef.current = resizePreview;
  const [startOpen, setStartOpen] = useState(false);
  const [selectedIcon, setSelectedIcon] = useState<string | null>(null);
  // Open context menu: { x, y, items } in desktop-local logical pixels, or null.
  const [menu, setMenu] = useState<MenuState | null>(null);
  // Desktop icons are CSS-flowed (fixed grid), so "Arrange Icons" re-sorts their
  // render order rather than moving free positions. `iconOrder` holds ids in
  // display order; `null` means the source array order.
  const [iconOrder, setIconOrder] = useState<string[] | null>(null);
  // Ids hidden by a shell-icon "Delete". These are shortcuts, not filesystem
  // entries, so removal is view-only and non-persistent (returns on relaunch).
  const [hiddenIcons, setHiddenIcons] = useState<Set<string>>(() => new Set());
  const [properties, setProperties] = useState<PropertiesState | null>(null);
  const listedApps = useMemo(() => apps(), []);
  const closeMenu = useCallback(() => setMenu(null), []);
  // Opening a context menu also dismisses the Start menu (only one popup at a time).
  const openMenu = useCallback((x: number, y: number, items: DesktopMenuItem[]) => { setStartOpen(false); setProperties(null); setMenu({ x, y, items }); }, []);
  const clearResizePreview = useCallback((id: number) => {
    setResizePreview((map) => {
      if (!map.has(id)) return map;
      const next = new Map(map);
      next.delete(id);
      resizePreviewRef.current = next;
      return next;
    });
  }, []);

  // Taskbar buttons keep a STABLE order (surface insertion order), unlike
  // `open` which is z-ordered — `activate` splices the focused window to the
  // end of `open` to paint it on top. Feeding that z-order to the taskbar makes
  // every focus change reshuffle the buttons, so the active button jumps
  // position and reads as "focus not tracking the window". Sorting by each
  // surface's original id (assigned in open order) pins the buttons; only the
  // `activeId` highlight moves, matching the Windows Classic taskbar.
  const taskbarWindows = useMemo(
    () => open.slice().sort((a, b) => a.id - b.id),
    [open],
  );

  // Native `surfaces()` is authoritative for existence and metadata. React
  // owns persistent frame geometry and z-order, so reconciliation must not
  // restore the native registry's launch-time bounds after a resize.
  const reconcile = useCallback((items: LiteSurface[]) => {
    return reconcileSurfaces(items, surfaces());
  }, []);

  const activate = useCallback((id: number) => {
    focus(id);
    setActiveId(id);
    setStartOpen(false);
    setMenu(null);
    // Raise to front: windows paint in `open` order, so moving the activated
    // surface to the end of the array draws it on top of the others.
    setOpen((items) => {
      const index = items.findIndex((item) => item.id === id);
      if (index < 0 || index === items.length - 1) return items;
      const next = items.slice();
      const [surface] = next.splice(index, 1);
      next.push(surface);
      return next;
    });
    // Activating from the taskbar is also the restore path for minimized windows.
    setMinimized((set) => {
      if (!set.has(id)) return set;
      const next = new Set(set);
      next.delete(id);
      return next;
    });
  }, []);

  useEffect(() => {
    const unsubscribe = globalThis.liteDesktopSubscribe((event) => {
      setOpen((items) => reconcile(items));
      if (event.type === "opened") setActiveId(event.surface.id);
      if (event.type === "activated") {
        // The compositor sends this on pointer-down on a foreign surface. A
        // minimized window has no visible surface to click, so an `activated`
        // naming a minimized id is a stale-routing race (the surface lingered
        // in the last-presented scene). Ignore it rather than yank the window
        // back — restoring is an explicit taskbar action via `activate`.
        if (!minimizedRef.current.has(event.surfaceId)) activate(event.surfaceId);
      }
      if (event.type === "moved") {
        setOpen((items) => applySurfaceMove(items, event.surfaceId, event.x, event.y));
      }
      if (event.type === "closed") {
        setMinimized((set) => {
          const next = new Set(set);
          next.delete(event.surfaceId);
          return next;
        });
        setMaximized((map) => {
          const next = new Map(map);
          next.delete(event.surfaceId);
          return next;
        });
        clearResizePreview(event.surfaceId);
        setActiveId((current) => (current === event.surfaceId ? 0 : current));
      }
    });
    return unsubscribe;
  }, []);

  // Global chords. A match key-grabs the chord: the whole sequence (Tab
  // downs/repeats, Shift transitions, the final Alt up) streams to this
  // desktop's root onKeyDown until every chord key is released.
  useEffect(() => {
    setAccelerators([
      { modifiers: MOD_ALT, code: KEY_TAB },
      { modifiers: MOD_ALT, code: KEY_F4 },
      { modifiers: MOD_CTRL, code: KEY_ESC },
    ]);
  }, []);

  // A window opening or closing mid-grab changes the candidate list; clamp the
  // selection (and drop the panel when nothing remains) before Alt is released.
  useEffect(() => {
    setSwitcher((state) => (state ? reconcileSwitcher(state, open) : state));
  }, [open]);

  const launchApp = useCallback((id: string) => { launch(id); setStartOpen(false); }, []);
  const closeWindow = useCallback((id: number) => {
    close(id);
    clearResizePreview(id);
    setOpen((items) => items.filter((item) => item.id !== id));
  }, [clearResizePreview]);
  // Root key handler: plain Escape dismissal plus the grabbed accelerator
  // sequences (Alt+Tab switcher, Alt+F4, Ctrl+Esc). Refs keep the reads current
  // when the final Alt-up lands before the last Tab cycle has rendered.
  const onDesktopKey = (rawEvent: unknown) => {
    const event = rawEvent as LiteKeyEvent;
    // Ctrl+Esc toggles the Start menu like the Start button (press edge only;
    // the chord grab also delivers the repeat and the key-ups).
    if (event.code === KEY_ESC && event.modifiers === MOD_CTRL) {
      if (event.value === 1) {
        setMenu(null);
        setStartOpen((value) => !value);
      }
      return;
    }
    if (event.code === KEY_ESC && event.value !== 0 && event.modifiers === 0) {
      setMenu(null);
      setStartOpen(false);
      setProperties(null);
      return;
    }
    // Grabbed Alt+Tab sequence: the first Tab down opens the panel, every
    // further Tab down/repeat cycles (Shift walks backwards).
    if (event.code === KEY_TAB && event.value !== 0 && (event.modifiers & MOD_ALT) !== 0) {
      const backward = (event.modifiers & MOD_SHIFT) !== 0;
      setSwitcher((state) => state ? cycle(state, backward) : openSwitcher(openRef.current));
      return;
    }
    // Releasing either Alt key commits the selection and closes the panel.
    if ((event.code === KEY_LEFTALT || event.code === KEY_RIGHTALT) && event.value === 0) {
      const state = switcherRef.current;
      if (state) {
        setSwitcher(null);
        const target = selectedCandidate(state);
        if (openRef.current.some((surface) => surface.id === target.id)) activate(target.id);
      }
      return;
    }
    // Alt+F4 closes the active window on the press edge only: a repeat would
    // re-close while the compositor's closed event is still in flight.
    if (event.code === KEY_F4 && event.value === 1 && (event.modifiers & MOD_ALT) !== 0) {
      const id = activeIdRef.current;
      if (id !== 0 && openRef.current.some((surface) => surface.id === id)) closeWindow(id);
    }
  };
  const minimizeWindow = useCallback((id: number) => {
    const next = new Set(minimizedRef.current);
    next.add(id);
    setMinimized(next);
    // If the minimized window was active, fall focus back to the top-most
    // still-visible window. Reading live refs (not stale closures) guarantees
    // the fallback is a surface that still exists.
    setActiveId((current) => {
      if (current !== id) return current;
      const fallback = openRef.current
        .filter((item) => item.id !== id && !next.has(item.id))
        .at(-1);
      const fallbackId = fallback ? fallback.id : 0;
      focus(fallbackId);
      return fallbackId;
    });
  }, []);
  const toggleMaximize = useCallback((id: number) => {
    setMaximized((map) => {
      const next = new Map(map);
      if (next.has(id)) {
        next.delete(id);
      } else {
        const surface = open.find((item) => item.id === id);
        if (surface) next.set(id, surface.bounds);
      }
      return next;
    });
  }, [open]);
  // XP taskbar toggle: clicking the already-active button minimizes its window;
  // any other click activates (and restores) its window.
  const toggleTask = useCallback((id: number) => {
    if (id === activeId && !minimizedRef.current.has(id)) {
      minimizeWindow(id);
    } else {
      activate(id);
    }
  }, [activeId, activate, minimizeWindow]);
  // Shared window system menu (taskbar button and titlebar right-click); the
  // enable/disable rules come from the pure `systemMenuItems` model.
  const openSystemMenu = useCallback((id: number, x: number, y: number) => {
    const state = { minimized: minimizedRef.current.has(id), maximized: maximized.has(id) };
    const actions: Record<string, (() => void) | undefined> = {
      restore: () => activate(id),
      minimize: () => minimizeWindow(id),
      maximize: () => toggleMaximize(id),
      close: () => closeWindow(id),
    };
    openMenu(x, y, systemMenuItems(state).map((item) => ({ ...item, onSelect: actions[item.id] })));
  }, [maximized, activate, minimizeWindow, toggleMaximize, closeWindow, openMenu]);
  const moveWindow = useCallback((id: number, x: number, y: number) => {
    // Dragging a maximized titlebar restores the window centered on the cursor.
    const restored = maximized.get(id);    if (restored) {
      setMaximized((map) => {
        const next = new Map(map);
        next.delete(id);
        return next;
      });
      const position = { x: clampX(x - Math.floor(restored.width / 2), restored.width), y: clampY(y - 12, restored.height) };
      move(id, position.x, position.y);
      setOpen((items) => items.map((item) => (item.id === id ? { ...item, bounds: { ...restored, ...position } } : item)));
      return;
    }
    setOpen((items) => items.map((item) => {
      if (item.id !== id) return item;
      const next = { x: clampX(x, item.bounds.width), y: clampY(y, item.bounds.height) };
      move(id, next.x, next.y);
      return { ...item, bounds: { ...item.bounds, ...next } };
    }));
  }, [maximized]);
  const beginWindowMove = useCallback((id: number, serial: number) => {
    // Restoring a maximized window changes canonical geometry first, so that
    // uncommon transition keeps the React path. Ordinary moves stay entirely
    // in compositor until the final `moved` event.
    if (maximized.has(id)) return false;
    const surface = openRef.current.find((item) => item.id === id);
    if (!surface) return false;
    beginMove(
      id,
      serial,
      0,
      0,
      Math.max(0, WORK_AREA.width - surface.bounds.width),
      Math.max(0, WORK_AREA.height - surface.bounds.height),
    );
    return true;
  }, [maximized]);
  // Resize preview throttle. Pointer motion only updates an outline; the
  // canonical window geometry and app configure commit once on release.
  const resizeThrottle = useRef<{ timer: ReturnType<typeof setTimeout> | null; pending: { id: number; rect: ResizeCandidate } | null }>({ timer: null, pending: null });
  // ~60 Hz. Long enough to collapse a burst of motions into one preview, short
  // enough that the resize still tracks the cursor smoothly.
  const RESIZE_FRAME_MS = 16;
  const publishResizePreview = useCallback((id: number, rect: ResizeCandidate) => {
    const bounds = constrainResize(rect, WORK_AREA, MIN_W, MIN_H);
    setResizePreview((map) => {
      const next = new Map(map);
      next.set(id, bounds);
      resizePreviewRef.current = next;
      return next;
    });
  }, []);
  const commitResize = useCallback((id: number, { x, y, width, height }: Rect) => {
    move(id, x, y);
    setOpen((items) => items.map((item) => (item.id === id ? { ...item, bounds: { x, y, width, height } } : item)));
    clearResizePreview(id);
  }, [clearResizePreview]);
  const resizeWindow = useCallback((id: number, rect: ResizeCandidate) => {
    const throttle = resizeThrottle.current;
    if (throttle.timer === null) {
      publishResizePreview(id, rect);
      throttle.timer = setTimeout(function tick() {
        const next = resizeThrottle.current.pending;
        if (next) {
          resizeThrottle.current.pending = null;
          publishResizePreview(next.id, next.rect);
          resizeThrottle.current.timer = setTimeout(tick, RESIZE_FRAME_MS);
        } else {
          resizeThrottle.current.timer = null;
        }
      }, RESIZE_FRAME_MS);
    } else {
      // Inside the frame window: stash only the newest rect.
      throttle.pending = { id, rect };
    }
  }, [publishResizePreview]);
  const flushResize = useCallback((id: number) => {
    const throttle = resizeThrottle.current;
    if (throttle.timer !== null) {
      clearTimeout(throttle.timer);
      throttle.timer = null;
    }
    let finalBounds = resizePreviewRef.current.get(id);
    if (throttle.pending && throttle.pending.id === id) {
      finalBounds = constrainResize(throttle.pending.rect, WORK_AREA, MIN_W, MIN_H);
      throttle.pending = null;
    }
    if (finalBounds) commitResize(id, finalBounds);
  }, [commitResize]);

  // Visible desktop icons in display order: hidden shell icons removed, then
  // ordered by `iconOrder` when "Arrange Icons" has sorted them.
  const visibleIcons = useMemo(() => {
    const shown = desktopIcons.filter((item) => !hiddenIcons.has(item.id));
    if (!iconOrder) return shown;
    const rank = new Map(iconOrder.map((id, index) => [id, index]));
    return shown.slice().sort((a, b) => (rank.get(a.id) ?? 0) - (rank.get(b.id) ?? 0));
  }, [hiddenIcons, iconOrder]);

  // Refresh re-runs the authoritative surface merge; Arrange re-sorts the fixed
  // icon grid alphabetically (there are no free positions to reset).
  const refreshDesktop = useCallback(() => setOpen(reconcile), [reconcile]);
  const arrangeIcons = useCallback(() => {
    setIconOrder(
      desktopIcons.map((item) => item.id).sort((a, b) => {
        const la = desktopIcons.find((i) => i.id === a)?.label ?? a;
        const lb = desktopIcons.find((i) => i.id === b)?.label ?? b;
        return la < lb ? -1 : 1;
      }),
    );
  }, []);
  const openProperties = useCallback((x: number, y: number, title: string, rows: [string, string][]) => {
    setStartOpen(false);
    setMenu(null);
    setProperties({ x, y, title, rows });
  }, []);

  return (
    <div
      id="desktop"
      onClick={() => { setSelectedIcon(null); setMenu(null); setStartOpen(false); setProperties(null); }}
      onContextMenu={(rawEvent) => {
        const event = rawEvent as unknown as LitePointerEvent;
        openMenu(event.x, event.y, [
          { id: "arrange", label: "Arrange Icons", onSelect: arrangeIcons },
          { id: "refresh", label: "Refresh", onSelect: refreshDesktop },
          { id: "properties", label: "Properties", onSelect: () => openProperties(event.x, event.y, "Desktop", [["Type", "Desktop"], ["Icons", String(visibleIcons.length)]]) },
        ]);
      }}
      onKeyDown={onDesktopKey}
    >
      <div className="desktop-icons">
        {visibleIcons.map((item) => (
          <div
            key={item.id}
            className="desktop-icon"
            onClick={() => setSelectedIcon(item.id)}
            onDoubleClick={() => item.app && launchApp(item.app)}
            onContextMenu={(rawEvent) => {
              const event = rawEvent as unknown as LitePointerEvent;
              setSelectedIcon(item.id);
              openMenu(event.x, event.y, [
                { id: "open", label: "Open", onSelect: () => item.app && launchApp(item.app) },
                { id: "delete", label: "Delete", onSelect: () => setHiddenIcons((set) => new Set(set).add(item.id)) },
                { id: "properties", label: "Properties", onSelect: () => openProperties(event.x, event.y, item.label, [["Type", item.app ? "Shortcut" : "System Folder"], ["Opens", item.app ?? "—"]]) },
              ]);
            }}
          >
            <img className="desktop-icon__image" src={item.icon}/>
            <span className={selectedIcon === item.id ? "desktop-icon__label desktop-icon__label--selected" : "desktop-icon__label"}>{item.label}</span>
          </div>
        ))}
      </div>
      {open.filter((surface) => !minimized.has(surface.id)).map((surface) => {
        const bounds = maximized.has(surface.id) ? WORK_AREA : surface.bounds;
        return (
          <Window key={surface.id} id={surface.id} title={surface.title} icon={surface.icon} active={surface.id === activeId} bounds={bounds} onActivate={activate} onClose={closeWindow} onMoveStart={beginWindowMove} onMove={moveWindow} onResize={resizeWindow} onResizeEnd={flushResize} onMinimize={minimizeWindow} onToggleMaximize={toggleMaximize} onSystemMenu={openSystemMenu} maximized={maximized.has(surface.id)}>
            <div className="client-surface" data-lite-surface={true} data-surface-id={surface.id} data-configure-serial={configure(surface.id, bounds.width - 10, bounds.height - 32)} />
          </Window>
        );
      })}
      {Array.from(resizePreview, ([id, bounds]) => (
        <React.Fragment key={id}>
          <div className="window__resize-preview window__resize-preview--horizontal" style={{ left: bounds.x, top: bounds.y, width: bounds.width }}/>
          <div className="window__resize-preview window__resize-preview--horizontal" style={{ left: bounds.x, top: bounds.y + bounds.height - 1, width: bounds.width }}/>
          <div className="window__resize-preview window__resize-preview--vertical" style={{ left: bounds.x, top: bounds.y, height: bounds.height }}/>
          <div className="window__resize-preview window__resize-preview--vertical" style={{ left: bounds.x + bounds.width - 1, top: bounds.y, height: bounds.height }}/>
        </React.Fragment>
      ))}
      {startOpen && <StartMenu apps={listedApps} onLaunch={launchApp} onShutdown={shutdown}/>}
      {menu && <ContextMenu x={menu.x} y={menu.y} items={menu.items} onClose={closeMenu}/>}
      {properties && <PropertiesPopup x={properties.x} y={properties.y} title={properties.title} rows={properties.rows} onClose={() => setProperties(null)}/>}
      {switcher && <AppSwitcher state={switcher}/>}
      <Taskbar windows={taskbarWindows} activeId={activeId} startOpen={startOpen} onStart={() => { setMenu(null); setStartOpen((value) => !value); }} onActivate={toggleTask} onTaskMenu={openSystemMenu}/>
    </div>
  );
}
