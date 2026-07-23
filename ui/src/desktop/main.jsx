import React, { useCallback, useEffect, useMemo, useState } from "react";
import { apps, launch } from "lite:apps";
import { close, configure, focus, move, surfaces, shutdown } from "lite:desktop";
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
const clampX = (x, width) => Math.max(0, Math.min(WORK_AREA.width - width, x));
const clampY = (y) => Math.max(0, Math.min(WORK_AREA.height - 25, y));

export default function Desktop() {
  const [open, setOpen] = useState(() => surfaces());
  const [activeId, setActiveId] = useState(() => open.at(-1)?.id ?? 0);
  const [minimized, setMinimized] = useState(() => new Set());
  // id -> bounds saved when the window was maximized; restore reads them back.
  const [maximized, setMaximized] = useState(() => new Map());
  const [startOpen, setStartOpen] = useState(false);
  const [selectedIcon, setSelectedIcon] = useState(null);
  const listedApps = useMemo(() => apps(), []);

  useEffect(() => {
    const unsubscribe = globalThis.liteDesktopSubscribe((event) => {
      setOpen(surfaces());
      if (event.type === "opened") setActiveId(event.surface.id);
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

  const activate = useCallback((id) => {
    focus(id);
    setActiveId(id);
    setStartOpen(false);
    // Activating from the taskbar is also the restore path for minimized windows.
    setMinimized((set) => {
      if (!set.has(id)) return set;
      const next = new Set(set);
      next.delete(id);
      return next;
    });
  }, []);
  const launchApp = useCallback((id) => { launch(id); setStartOpen(false); }, []);
  const closeWindow = useCallback((id) => { close(id); setOpen((items) => items.filter((item) => item.id !== id)); }, []);
  const minimizeWindow = useCallback((id) => {
    const next = new Set(minimized);
    next.add(id);
    setMinimized(next);
    if (activeId === id) {
      const fallback = open.filter((item) => item.id !== id && !next.has(item.id)).at(-1);
      focus(fallback ? fallback.id : 0);
      setActiveId(fallback ? fallback.id : 0);
    }
  }, [open, minimized, activeId]);
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
      const position = { x: clampX(x - Math.floor(restored.width / 2), restored.width), y: clampY(y - 12) };
      move(id, position.x, position.y);
      setOpen((items) => items.map((item) => (item.id === id ? { ...item, bounds: { ...restored, ...position } } : item)));
      return;
    }
    setOpen((items) => items.map((item) => {
      if (item.id !== id) return item;
      const next = { x: clampX(x, item.bounds.width), y: clampY(y) };
      move(id, next.x, next.y);
      return { ...item, bounds: { ...item.bounds, ...next } };
    }));
  }, [maximized]);

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
          <Window key={surface.id} id={surface.id} title={surface.title} icon={surface.icon} active={surface.id === activeId} bounds={bounds} onActivate={activate} onClose={closeWindow} onMove={moveWindow} onMinimize={minimizeWindow} onToggleMaximize={toggleMaximize} maximized={maximized.has(surface.id)}>
            <surface className="client-surface" id={surface.id} configureSerial={configure(surface.id, bounds.width - 8, bounds.height - 33)} frame={bounds} cornerRadius={8} />
          </Window>
        );
      })}
      {startOpen && <StartMenu apps={listedApps} onLaunch={launchApp} onShutdown={shutdown}/>} 
      <Taskbar windows={open} activeId={activeId} startOpen={startOpen} onStart={() => setStartOpen((value) => !value)} onActivate={activate}/>
    </view>
  );
}
