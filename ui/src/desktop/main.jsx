import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { apps, launch } from "lite:apps";
import { beginMove, close, configure, focus, move, surfaces, shutdown } from "lite:desktop";
import { Window } from "../design-system/window.jsx";
import { Taskbar } from "../design-system/taskbar.jsx";
import { StartMenu } from "../design-system/start-menu.jsx";
import { ContextMenu } from "../design-system/context-menu.jsx";
import { constrainResize } from "../design-system/window-geometry.js";
import { applySurfaceMove, reconcileSurfaces } from "./surface-state.js";

const desktopIcons = [
  { id: "computer", label: "My Computer", icon: "assets/computer.png" },
  { id: "terminal", label: "Terminal", icon: "assets/terminal.png", app: "terminal" },
  { id: "documents", label: "My Documents", icon: "assets/documents.png" },
  { id: "trash", label: "Recycle Bin", icon: "assets/trash.png" },
];

// Right-click menu on the desktop background. Items are placeholders for now
// (no backing actions yet); every click dismisses the menu.
const DESKTOP_MENU_ITEMS = [
  { id: "arrange", label: "Arrange Icons" },
  { id: "refresh", label: "Refresh" },
  { id: "properties", label: "Properties" },
];

// Linux evdev KEY_ESC. Escape dismisses open popups when the desktop is focused.
const KEY_ESC = 1;

// The taskbar-free area every maximized window covers; move clamps agree.
const WORK_AREA = { x: 0, y: 0, width: 1504, height: 816 };
// Minimum window frame size. Comfortably above the classic chrome insets (10
// wide, 32 tall) so the client area stays usable and
// `configure(w - 10, h - 32)` never underflows the u32 the host parses.
const MIN_W = 160;
const MIN_H = 120;
const clampX = (x, width) => Math.max(0, Math.min(WORK_AREA.width - width, x));
const clampY = (y, height) => Math.max(0, Math.min(WORK_AREA.height - height, y));

export default function Desktop() {
  const [open, setOpen] = useState(() => surfaces());
  // Live mirror of `open` so stable callbacks read current z-order without a
  // stale closure (see `minimizedRef`).
  const openRef = useRef(open);
  openRef.current = open;
  const [activeId, setActiveId] = useState(() => open.at(-1)?.id ?? 0);
  const [minimized, setMinimized] = useState(() => new Set());
  // Live mirror of `minimized` so the stable `[]`-deps subscribe callback and
  // callbacks can read the current set without a stale closure or a state
  // setter's side effects.
  const minimizedRef = useRef(minimized);
  minimizedRef.current = minimized;
  // id -> bounds saved when the window was maximized; restore reads them back.
  const [maximized, setMaximized] = useState(() => new Map());
  // id -> outline bounds shown during classic resize. Keeping this separate
  // from `open` prevents every pointer motion from configuring a new app
  // buffer; without it the gray window body and app pixels alternate onscreen.
  const [resizePreview, setResizePreview] = useState(() => new Map());
  const resizePreviewRef = useRef(resizePreview);
  resizePreviewRef.current = resizePreview;
  const [startOpen, setStartOpen] = useState(false);
  const [selectedIcon, setSelectedIcon] = useState(null);
  // Open context menu: { x, y, items } in desktop-local logical pixels, or null.
  const [menu, setMenu] = useState(null);
  const listedApps = useMemo(() => apps(), []);
  const closeMenu = useCallback(() => setMenu(null), []);
  // Opening a context menu also dismisses the Start menu (only one popup at a time).
  const openMenu = useCallback((x, y, items) => { setStartOpen(false); setMenu({ x, y, items }); }, []);
  const clearResizePreview = useCallback((id) => {
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
  const reconcile = useCallback((items) => {
    return reconcileSurfaces(items, surfaces());
  }, []);

  const activate = useCallback((id) => {
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
    if (surfaces().length === 0) launch("terminal");
    return unsubscribe;
  }, []);

  const launchApp = useCallback((id) => { launch(id); setStartOpen(false); }, []);
  const closeWindow = useCallback((id) => {
    close(id);
    clearResizePreview(id);
    setOpen((items) => items.filter((item) => item.id !== id));
  }, [clearResizePreview]);
  const minimizeWindow = useCallback((id) => {
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
  const toggleMaximize = useCallback((id) => {
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
  const moveWindow = useCallback((id, x, y) => {
    // Dragging a maximized titlebar restores the window centered on the cursor.
    const restored = maximized.get(id);
    if (restored) {
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
  const beginWindowMove = useCallback((id, serial) => {
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
  const resizeThrottle = useRef({ timer: null, pending: null });
  // ~60 Hz. Long enough to collapse a burst of motions into one preview, short
  // enough that the resize still tracks the cursor smoothly.
  const RESIZE_FRAME_MS = 16;
  const publishResizePreview = useCallback((id, rect) => {
    const bounds = constrainResize(rect, WORK_AREA, MIN_W, MIN_H);
    setResizePreview((map) => {
      const next = new Map(map);
      next.set(id, bounds);
      resizePreviewRef.current = next;
      return next;
    });
  }, []);
  const commitResize = useCallback((id, { x, y, width, height }) => {
    move(id, x, y);
    setOpen((items) => items.map((item) => (item.id === id ? { ...item, bounds: { x, y, width, height } } : item)));
    clearResizePreview(id);
  }, [clearResizePreview]);
  const resizeWindow = useCallback((id, rect) => {
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
  const flushResize = useCallback((id) => {
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

  return (
    <view
      id="desktop"
      onClick={() => { setSelectedIcon(null); setMenu(null); setStartOpen(false); }}
      onContextMenu={(event) => openMenu(event.x, event.y, DESKTOP_MENU_ITEMS)}
      onKeyDown={(event) => { if (event.code === KEY_ESC && event.value !== 0) { setMenu(null); setStartOpen(false); } }}
    >
      <view className="desktop-icons">
        {desktopIcons.map((item) => (
          <view
            key={item.id}
            className="desktop-icon"
            onClick={() => setSelectedIcon(item.id)}
            onDoubleClick={() => item.app && launchApp(item.app)}
            onContextMenu={(event) => {
              setSelectedIcon(item.id);
              openMenu(event.x, event.y, [
                { id: "open", label: "Open", onSelect: () => item.app && launchApp(item.app) },
                { id: "delete", label: "Delete" },
                { id: "properties", label: "Properties" },
              ]);
            }}
          >
            <image className="desktop-icon__image" src={item.icon}/>
            <text className={selectedIcon === item.id ? "desktop-icon__label desktop-icon__label--selected" : "desktop-icon__label"}>{item.label}</text>
          </view>
        ))}
      </view>
      {open.filter((surface) => !minimized.has(surface.id)).map((surface) => {
        const bounds = maximized.has(surface.id) ? WORK_AREA : surface.bounds;
        return (
          <Window key={surface.id} id={surface.id} title={surface.title} icon={surface.icon} active={surface.id === activeId} bounds={bounds} onActivate={activate} onClose={closeWindow} onMoveStart={beginWindowMove} onMove={moveWindow} onResize={resizeWindow} onResizeEnd={flushResize} onMinimize={minimizeWindow} onToggleMaximize={toggleMaximize} maximized={maximized.has(surface.id)}>
            <surface className="client-surface" id={surface.id} configureSerial={configure(surface.id, bounds.width - 10, bounds.height - 32)} frame={bounds} cornerRadius={0} />
          </Window>
        );
      })}
      {Array.from(resizePreview, ([id, bounds]) => (
        <React.Fragment key={id}>
          <view className="window__resize-preview window__resize-preview--horizontal" style={{ left: bounds.x, top: bounds.y, width: bounds.width }} overlay={true}/>
          <view className="window__resize-preview window__resize-preview--horizontal" style={{ left: bounds.x, top: bounds.y + bounds.height - 1, width: bounds.width }} overlay={true}/>
          <view className="window__resize-preview window__resize-preview--vertical" style={{ left: bounds.x, top: bounds.y, height: bounds.height }} overlay={true}/>
          <view className="window__resize-preview window__resize-preview--vertical" style={{ left: bounds.x + bounds.width - 1, top: bounds.y, height: bounds.height }} overlay={true}/>
        </React.Fragment>
      ))}
      {startOpen && <StartMenu apps={listedApps} onLaunch={launchApp} onShutdown={shutdown}/>}
      {menu && <ContextMenu x={menu.x} y={menu.y} items={menu.items} onClose={closeMenu}/>}
      <Taskbar windows={taskbarWindows} activeId={activeId} startOpen={startOpen} onStart={() => { setMenu(null); setStartOpen((value) => !value); }} onActivate={activate}/>
    </view>
  );
}
