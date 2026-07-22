import React, { useCallback, useEffect, useMemo, useState } from "react";
import { apps, launch } from "lite:apps";
import { close, configure, focus, move, surfaces, shutdown } from "lite:desktop";
import { Window } from "../design-system/window.jsx";
import { Taskbar } from "../design-system/taskbar.jsx";
import { StartMenu } from "../design-system/start-menu.jsx";

const desktopIcons = [
  { id: "computer", label: "My Computer" },
  { id: "terminal", label: "Terminal", app: "terminal" },
  { id: "documents", label: "My Documents" },
  { id: "trash", label: "Recycle Bin" },
];

export default function Desktop() {
  const [open, setOpen] = useState(() => surfaces());
  const [activeId, setActiveId] = useState(() => open.at(-1)?.id ?? 0);
  const [startOpen, setStartOpen] = useState(false);
  const listedApps = useMemo(() => apps(), []);

  useEffect(() => {
    const unsubscribe = globalThis.liteDesktopSubscribe((event) => {
      setOpen(surfaces());
      if (event.type === "opened") setActiveId(event.surface.id);
    });
    if (surfaces().length === 0) launch("terminal");
    return unsubscribe;
  }, []);

  const activate = useCallback((id) => { focus(id); setActiveId(id); setStartOpen(false); }, []);
  const launchApp = useCallback((id) => { launch(id); setStartOpen(false); }, []);
  const closeWindow = useCallback((id) => { close(id); setOpen((items) => items.filter((item) => item.id !== id)); }, []);
  const moveWindow = useCallback((id, x, y) => {
    setOpen((items) => items.map((item) => {
      if (item.id !== id) return item;
      const next = {
        x: Math.max(0, Math.min(1504 - item.bounds.width, x)),
        y: Math.max(0, Math.min(816 - 25, y)),
      };
      move(id, next.x, next.y);
      return { ...item, bounds: { ...item.bounds, ...next } };
    }));
  }, []);

  return (
    <view id="desktop">
      <image className="wallpaper" src="assets/bliss.png" />
      <view className="desktop-icons">
        {desktopIcons.map((item) => <view key={item.id} className="desktop-icon" onDoubleClick={() => item.app && launchApp(item.app)}><view className={`system-icon system-icon--${item.id}`}/><text>{item.label}</text></view>)}
      </view>
      {open.map((surface, index) => (
        <Window key={surface.id} id={surface.id} title={surface.title} icon={surface.icon} active={surface.id === activeId} bounds={surface.bounds} onActivate={activate} onClose={closeWindow} onMove={moveWindow}>
          <surface className="client-surface" id={surface.id} configureSerial={configure(surface.id, surface.bounds.width - 6, surface.bounds.height - 31)} />
        </Window>
      ))}
      {startOpen && <StartMenu apps={listedApps} onLaunch={launchApp} onShutdown={shutdown}/>} 
      <Taskbar windows={open} activeId={activeId} startOpen={startOpen} onStart={() => setStartOpen((value) => !value)} onActivate={activate}/>
    </view>
  );
}
