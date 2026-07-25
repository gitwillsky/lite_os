import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { apps, launch } from "lite:apps";
import { beginMove, close, configure, focus, move, surfaces, shutdown } from "lite:desktop";
import { Window } from "../design-system/window.jsx";
import { Taskbar } from "../design-system/taskbar.jsx";
import { StartMenu } from "../design-system/start-menu.jsx";

const desktopIcons = [
  { id: "computer", label: "My Computer", icon: "assets/computer.png" },
  { id: "terminal", label: "Terminal", icon: "assets/terminal.png", app: "terminal" },
  { id: "documents", label: "My Documents", icon: "assets/documents.png" },
  { id: "trash", label: "Recycle Bin", icon: "assets/trash.png" },
];

// The taskbar-free area every maximized window covers; move clamps agree.
const WORK_AREA = { x: 0, y: 0, width: 1504, height: 816 };
// Minimum window frame size. Comfortably above the chrome insets (10 wide, 39
// tall) so the client area stays usable and `configure(w - 10, h - 39)` never
// underflows the u32 the host parses.
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
  const [startOpen, setStartOpen] = useState(false);
  const [selectedIcon, setSelectedIcon] = useState(null);
  const listedApps = useMemo(() => apps(), []);

  // Taskbar buttons keep a STABLE order (surface insertion order), unlike
  // `open` which is z-ordered — `activate` splices the focused window to the
  // end of `open` to paint it on top. Feeding that z-order to the taskbar makes
  // every focus change reshuffle the buttons, so the active button jumps
  // position and reads as "focus not tracking the window". Sorting by each
  // surface's original id (assigned in open order) pins the buttons; only the
  // `activeId` highlight moves, matching the Luna taskbar.
  const taskbarWindows = useMemo(
    () => open.slice().sort((a, b) => a.id - b.id),
    [open],
  );

  // Native `surfaces()` is authoritative for which surfaces EXIST (insertion
  // order), but z-order lives here in `open`: `activate` raises a surface by
  // moving it to the end of this array. Merging native truth into the current
  // order — instead of replacing it — preserves that raise-to-front. New
  // surfaces append (top), closed ones drop, and mutable fields (title/bounds)
  // refresh from native without disturbing z-order.
  const reconcile = useCallback((items) => {
    const native = surfaces();
    const byId = new Map(native.map((surface) => [surface.id, surface]));
    const kept = items
      .filter((item) => byId.has(item.id))
      .map((item) => byId.get(item.id));
    const keptIds = new Set(kept.map((item) => item.id));
    const added = native.filter((surface) => !keptIds.has(surface.id));
    return [...kept, ...added];
  }, []);

  const activate = useCallback((id) => {
    focus(id);
    setActiveId(id);
    setStartOpen(false);
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
        setActiveId((current) => (current === event.surfaceId ? 0 : current));
      }
    });
    if (surfaces().length === 0) launch("terminal");
    return unsubscribe;
  }, []);

  const launchApp = useCallback((id) => { launch(id); setStartOpen(false); }, []);
  const closeWindow = useCallback((id) => { close(id); setOpen((items) => items.filter((item) => item.id !== id)); }, []);
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
  // Resize commit throttle. Every pointer motion on an edge fires `onResize`,
  // and each commit does a full `move()` + `setOpen()` (one scene commit) plus
  // a `configure()` at render (one new configure serial per distinct size). At
  // pointer rate that floods the compositor with per-motion re-renders. The
  // runtime exposes no requestAnimationFrame and no present signal to pace on —
  // only `setTimeout` — so coalesce here: commit the leading motion at once for
  // responsiveness, collapse interior motions to the newest rect, and flush the
  // trailing rect one frame later. `flushResize` (called on pointer-up) commits
  // whatever the last interval deferred so the final size is never dropped.
  const resizeThrottle = useRef({ timer: null, pending: null });
  // ~60 Hz. Long enough to collapse a burst of motions into one commit, short
  // enough that the resize still tracks the cursor smoothly.
  const RESIZE_FRAME_MS = 16;
  const commitResize = useCallback((id, rect) => {
    // Dragging any edge of a maximized window first restores it, then resizes.
    if (maximized.has(id)) {
      setMaximized((map) => {
        const next = new Map(map);
        next.delete(id);
        return next;
      });
    }
    let { x, y, width, height, anchorRight, anchorBottom, right, bottom } = rect;
    // Clamp to the min size. When a left/top edge drives the drag it also moved
    // the origin, so pin the far edge (right/bottom) rather than let the origin
    // keep sliding once the min size is hit.
    if (width < MIN_W) { width = MIN_W; if (anchorRight) x = right - MIN_W; }
    if (height < MIN_H) { height = MIN_H; if (anchorBottom) y = bottom - MIN_H; }
    // Keep the window inside the taskbar-free work area.
    x = Math.max(0, x);
    y = Math.max(0, y);
    width = Math.min(width, WORK_AREA.width - x);
    height = Math.min(height, WORK_AREA.height - y);
    width = Math.max(MIN_W, width);
    height = Math.max(MIN_H, height);
    move(id, x, y);
    setOpen((items) => items.map((item) => (item.id === id ? { ...item, bounds: { x, y, width, height } } : item)));
  }, [maximized]);
  const resizeWindow = useCallback((id, rect) => {
    const throttle = resizeThrottle.current;
    if (throttle.timer === null) {
      // Leading edge: commit immediately, then open a frame window that
      // collapses any motions arriving inside it into a single trailing commit.
      commitResize(id, rect);
      throttle.timer = setTimeout(function tick() {
        const next = resizeThrottle.current.pending;
        if (next) {
          resizeThrottle.current.pending = null;
          commitResize(next.id, next.rect);
          // A commit landed this frame; keep the window open one more frame in
          // case more motions coalesced meanwhile, so the drag stays smooth.
          resizeThrottle.current.timer = setTimeout(tick, RESIZE_FRAME_MS);
        } else {
          resizeThrottle.current.timer = null;
        }
      }, RESIZE_FRAME_MS);
    } else {
      // Inside the frame window: stash only the newest rect.
      throttle.pending = { id, rect };
    }
  }, [commitResize]);
  const flushResize = useCallback((id) => {
    const throttle = resizeThrottle.current;
    if (throttle.timer !== null) {
      clearTimeout(throttle.timer);
      throttle.timer = null;
    }
    // Commit the final deferred rect so the window ends exactly where the
    // pointer released, not at the last frame boundary.
    if (throttle.pending && throttle.pending.id === id) {
      const { rect } = throttle.pending;
      throttle.pending = null;
      commitResize(id, rect);
    }
  }, [commitResize]);

  return (
    <view id="desktop" onClick={() => setSelectedIcon(null)}>
      <image className="wallpaper" src="assets/bliss.png" />
      <view className="desktop-icons">
        {desktopIcons.map((item) => (
          <view key={item.id} className="desktop-icon" onClick={() => setSelectedIcon(item.id)} onDoubleClick={() => item.app && launchApp(item.app)}>
            <image className="desktop-icon__image" src={item.icon}/>
            <text className={selectedIcon === item.id ? "desktop-icon__label desktop-icon__label--selected" : "desktop-icon__label"}>{item.label}</text>
          </view>
        ))}
      </view>
      {open.filter((surface) => !minimized.has(surface.id)).map((surface) => {
        const bounds = maximized.has(surface.id) ? WORK_AREA : surface.bounds;
        return (
          <Window key={surface.id} id={surface.id} title={surface.title} icon={surface.icon} active={surface.id === activeId} bounds={bounds} onActivate={activate} onClose={closeWindow} onMoveStart={beginWindowMove} onMove={moveWindow} onResize={resizeWindow} onResizeEnd={flushResize} onMinimize={minimizeWindow} onToggleMaximize={toggleMaximize} maximized={maximized.has(surface.id)}>
            <surface className="client-surface" id={surface.id} configureSerial={configure(surface.id, bounds.width - 10, bounds.height - 39)} frame={bounds} cornerRadius={8} />
          </Window>
        );
      })}
      {startOpen && <StartMenu apps={listedApps} onLaunch={launchApp} onShutdown={shutdown}/>} 
      <Taskbar windows={taskbarWindows} activeId={activeId} startOpen={startOpen} onStart={() => setStartOpen((value) => !value)} onActivate={activate}/>
    </view>
  );
}
