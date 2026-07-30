import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { apps, launch } from "lite:apps";
import { getState, setMuted, setVolume, subscribe } from "lite:audio-system";
import {
  beginMove,
  close,
  configure,
  focus,
  move,
  setAccelerators,
  shutdown,
  surfaces,
} from "lite:desktop";
import { Window } from "../design-system/window.tsx";
import { Dock, CommandCenter, SystemCenter, TopBar, WorkspaceOverview } from "../design-system/shell.tsx";
import type { ShellPanel } from "../design-system/shell.tsx";
import { constrainResize, frameStyle } from "../design-system/window-geometry.ts";
import type { Rect, ResizeCandidate } from "../design-system/window-geometry.ts";
import { applySurfaceMove, fitSurfaceFrame, reconcileSurfaces } from "./surface-state.ts";
import { Splash } from "./splash.tsx";

const MIN_WINDOW = { width: 260, height: 180 };
const KEY_ESC = 1;
const KEY_TAB = 15;
const KEY_F4 = 62;
const MOD_ALT = 4;
const WORKSPACE_COUNT = 3;

const dockApps = [
  { id: "file-manager", label: "Files", icon: "assets/files.png", title: "Files" },
  { id: "terminal", label: "Terminal", icon: "assets/terminal.png", title: "Terminal" },
  { id: "music-player", label: "Music", icon: "assets/monitor.png", title: "Music" },
  { id: "my-computer", label: "Workspace", icon: "assets/package.png", title: "Computer" },
];

const appIcon = (id: string) => dockApps.find((item) => item.id === id)?.icon ?? "assets/package.png";

const viewport = () => ({ width: window.innerWidth, height: window.innerHeight });

const workArea = (screen: { width: number; height: number }): Rect => {
  const x = Math.min(12, Math.max(0, screen.width - 1));
  const y = Math.min(56, Math.max(0, screen.height - 55));
  return {
    x,
    y,
    width: Math.max(1, screen.width - x - Math.min(12, screen.width - x - 1)),
    height: Math.max(55, screen.height - y - Math.min(74, screen.height - y - 55)),
  };
};

export default function Desktop() {
  const [screen, setScreen] = useState(viewport);
  const desktopArea = useMemo(() => workArea(screen), [screen]);
  const [open, setOpen] = useState(() => surfaces());
  const openRef = useRef(open);
  openRef.current = open;
  const [activeId, setActiveId] = useState(() => open.at(-1)?.id ?? 0);
  const activeIdRef = useRef(activeId);
  activeIdRef.current = activeId;
  const [activeWorkspace, setActiveWorkspace] = useState(0);
  const activeWorkspaceRef = useRef(activeWorkspace);
  activeWorkspaceRef.current = activeWorkspace;
  const [surfaceWorkspace, setSurfaceWorkspace] = useState(
    () => new Map(open.map((surface) => [surface.id, 0])),
  );
  const surfaceWorkspaceRef = useRef(surfaceWorkspace);
  surfaceWorkspaceRef.current = surfaceWorkspace;
  const [minimized, setMinimized] = useState<Set<number>>(() => new Set());
  const minimizedRef = useRef(minimized);
  minimizedRef.current = minimized;
  const [maximized, setMaximized] = useState<Map<number, LiteFrame>>(() => new Map());
  const [resizePreview, setResizePreview] = useState<Map<number, Rect>>(() => new Map());
  const resizePreviewRef = useRef(resizePreview);
  resizePreviewRef.current = resizePreview;
  const [panel, setPanel] = useState<ShellPanel>(null);
  const [clock, setClock] = useState(() => new Date());
  const [master, setMaster] = useState({ percent: 75, muted: false });
  const booted = useRef(false);
  const listedApps = useMemo(() => apps(), []);

  useEffect(() => {
    if (!booted.current && surfaces().length === 0) {
      booted.current = true;
      launch("file-manager");
      launch("terminal");
    }
    let cancelled = false;
    let timer = setTimeout(function tick() {
      setClock(new Date());
      if (!cancelled) timer = setTimeout(tick, 30_000);
    }, 30_000);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, []);

  useEffect(() => {
    const resize = () => setScreen(viewport());
    window.addEventListener("resize", resize);
    return () => window.removeEventListener("resize", resize);
  }, []);

  useEffect(() => {
    setOpen((current) => current.map((surface) => {
      const bounds = fitSurfaceFrame(surface.bounds, desktopArea);
      if (
        bounds.x === surface.bounds.x
        && bounds.y === surface.bounds.y
        && bounds.width === surface.bounds.width
        && bounds.height === surface.bounds.height
      ) return surface;
      move(surface.id, bounds.x, bounds.y);
      return { ...surface, bounds };
    }));
  }, [desktopArea]);

  useEffect(() => {
    const unsubscribe = subscribe((state) => {
      setMaster({ percent: state.percent, muted: state.muted });
    });
    getState();
    return unsubscribe;
  }, []);

  const synchronizeActivation = useCallback((id: number) => {
    const workspace = surfaceWorkspaceRef.current.get(id);
    if (workspace !== undefined) setActiveWorkspace(workspace);
    focus(id);
    setActiveId(id);
    setMinimized((current) => {
      if (!current.has(id)) return current;
      const next = new Set(current);
      next.delete(id);
      return next;
    });
    setOpen((current) => {
      const index = current.findIndex((surface) => surface.id === id);
      if (index < 0 || index === current.length - 1) return current;
      const next = current.slice();
      const [surface] = next.splice(index, 1);
      next.push(surface);
      return next;
    });
  }, []);
  const activate = useCallback((id: number) => {
    synchronizeActivation(id);
    setPanel(null);
  }, [synchronizeActivation]);

  const closeWindow = useCallback((id: number) => {
    close(id);
    setOpen((current) => current.filter((surface) => surface.id !== id));
    setMinimized((current) => {
      const next = new Set(current);
      next.delete(id);
      return next;
    });
    setMaximized((current) => {
      const next = new Map(current);
      next.delete(id);
      return next;
    });
    setSurfaceWorkspace((current) => {
      const next = new Map(current);
      next.delete(id);
      return next;
    });
  }, []);

  useEffect(() => globalThis.liteDesktopSubscribe((event) => {
    const snapshot = surfaces();
    setOpen((current) => reconcileSurfaces(current, snapshot).map((surface) => {
      const bounds = fitSurfaceFrame(surface.bounds, desktopArea);
      if (bounds.x !== surface.bounds.x || bounds.y !== surface.bounds.y) {
        move(surface.id, bounds.x, bounds.y);
      }
      return { ...surface, bounds };
    }));
    setSurfaceWorkspace((current) => {
      const next = new Map(current);
      for (const surface of snapshot) {
        if (!next.has(surface.id)) next.set(surface.id, activeWorkspaceRef.current);
      }
      return next;
    });
    if (event.type === "opened") {
      setActiveId(event.surface.id);
    }
    if (event.type === "activated" && !minimizedRef.current.has(event.surfaceId)) {
      // App-surface activation is an asynchronous compositor reconciliation,
      // not a click through the current shell overlay. Closing `panel` here
      // lets a delayed activation from the prior launch dismiss a newly opened
      // Command Center; window chrome and the scrim already own synchronous
      // panel dismissal.
      synchronizeActivation(event.surfaceId);
    }
    if (event.type === "moved") {
      setOpen((current) => applySurfaceMove(current, event.surfaceId, event.x, event.y));
    }
    if (event.type === "closed") {
      setActiveId((current) => current === event.surfaceId ? 0 : current);
      setSurfaceWorkspace((current) => {
        const next = new Map(current);
        next.delete(event.surfaceId);
        return next;
      });
    }
  }), [desktopArea, synchronizeActivation]);

  useEffect(() => {
    setAccelerators([
      { modifiers: 0, code: KEY_ESC },
      { modifiers: MOD_ALT, code: KEY_TAB },
      { modifiers: MOD_ALT, code: KEY_F4 },
    ]);
  }, []);

  const onDesktopKey = (raw: unknown) => {
    const event = raw as LiteKeyEvent;
    if (event.code === KEY_ESC && event.value === 1) {
      setPanel(null);
    }
    if (event.code === KEY_F4 && event.value === 1 && (event.modifiers & MOD_ALT) !== 0) {
      const id = activeIdRef.current;
      if (id) closeWindow(id);
    }
    if (event.code === KEY_TAB && event.value === 1 && (event.modifiers & MOD_ALT) !== 0) {
      const candidates = openRef.current.filter((surface) =>
        surfaceWorkspaceRef.current.get(surface.id) === activeWorkspaceRef.current
        && !minimizedRef.current.has(surface.id),
      );
      const index = candidates.findIndex((surface) => surface.id === activeIdRef.current);
      const next = candidates[(index + 1) % candidates.length];
      if (next) activate(next.id);
    }
  };

  const launchOrActivate = useCallback((app: typeof dockApps[number]) => {
    const existing = openRef.current.find((surface) => surface.appId === app.id);
    existing ? activate(existing.id) : launch(app.id);
  }, [activate]);

  const minimizeWindow = useCallback((id: number) => {
    setMinimized((current) => new Set(current).add(id));
    if (activeIdRef.current === id) {
      const fallback = openRef.current
        .filter((surface) =>
          surface.id !== id
          && surfaceWorkspaceRef.current.get(surface.id) === activeWorkspaceRef.current
          && !minimizedRef.current.has(surface.id),
        )
        .at(-1);
      const next = fallback?.id ?? 0;
      focus(next);
      setActiveId(next);
    }
  }, []);

  const toggleMaximize = useCallback((id: number) => {
    setMaximized((current) => {
      const next = new Map(current);
      if (next.has(id)) {
        next.delete(id);
      } else {
        const surface = openRef.current.find((item) => item.id === id);
        if (surface) next.set(id, surface.bounds);
      }
      return next;
    });
  }, []);

  const beginWindowMove = useCallback((id: number, serial: number) => {
    if (maximized.has(id)) return;
    const surface = openRef.current.find((item) => item.id === id);
    if (!surface) return;
    beginMove(
      id,
      serial,
      desktopArea.x,
      desktopArea.y,
      desktopArea.x + desktopArea.width - surface.bounds.width,
      desktopArea.y + desktopArea.height - surface.bounds.height,
    );
  }, [desktopArea, maximized]);

  const resizeWindow = useCallback((id: number, candidate: ResizeCandidate) => {
    const bounds = constrainResize(
      candidate,
      desktopArea,
      Math.min(MIN_WINDOW.width, desktopArea.width),
      Math.min(MIN_WINDOW.height, desktopArea.height),
    );
    setResizePreview((current) => new Map(current).set(id, bounds));
  }, [desktopArea]);
  const finishResize = useCallback((id: number) => {
    const bounds = resizePreviewRef.current.get(id);
    if (!bounds) return;
    move(id, bounds.x, bounds.y);
    setOpen((current) => current.map((surface) =>
      surface.id === id ? { ...surface, bounds } : surface,
    ));
    setResizePreview((current) => {
      const next = new Map(current);
      next.delete(id);
      return next;
    });
  }, []);

  const commandApps = listedApps.map((app) => ({
    id: app.id,
    name: app.id === "file-manager" ? "Files" : app.name,
    icon: appIcon(app.id),
  }));
  const selectWorkspace = useCallback((workspace: number) => {
    setActiveWorkspace(workspace);
    activeWorkspaceRef.current = workspace;
    const next = openRef.current
      .filter((surface) =>
        surfaceWorkspaceRef.current.get(surface.id) === workspace
        && !minimizedRef.current.has(surface.id),
      )
      .at(-1);
    focus(next?.id ?? 0);
    setActiveId(next?.id ?? 0);
    setPanel(null);
  }, []);
  const visible = open.filter((surface) =>
    surfaceWorkspace.get(surface.id) === activeWorkspace && !minimized.has(surface.id),
  );
  const workspaceViews = Array.from({ length: WORKSPACE_COUNT }, (_, id) => ({
    id,
    windows: open.filter((surface) => surfaceWorkspace.get(surface.id) === id),
  }));
  const time = `${String(clock.getHours()).padStart(2, "0")}:${String(clock.getMinutes()).padStart(2, "0")}`;
  const weekdays = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
  const months = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
  const date = `${weekdays[clock.getDay()]}, ${months[clock.getMonth()]} ${clock.getDate()}`;

  return (
    <div id="desktop" className="aurora-root" onKeyDown={onDesktopKey}>
      <TopBar
        panel={panel}
        time={time}
        activeWorkspace={activeWorkspace}
        workspaceCount={WORKSPACE_COUNT}
        onPanel={setPanel}
      />
      {visible.map((surface) => {
        const bounds = maximized.has(surface.id) ? desktopArea : surface.bounds;
        return (
          <Window
            key={surface.id}
            id={surface.id}
            appId={surface.appId}
            title={surface.title}
            icon={surface.icon}
            active={surface.id === activeId}
            bounds={bounds}
            onActivate={activate}
            onClose={closeWindow}
            onMoveStart={beginWindowMove}
            onResize={resizeWindow}
            onResizeEnd={finishResize}
            onMinimize={minimizeWindow}
            onToggleMaximize={toggleMaximize}
            maximized={maximized.has(surface.id)}
          >
            <div
              className="client-surface"
              data-lite-surface={true}
              data-surface-id={surface.id}
              data-configure-serial={configure(surface.id, bounds.width - 4, bounds.height - 54)}
            />
          </Window>
        );
      })}
      {Array.from(resizePreview, ([id, bounds]) => (
        <div key={id} className="window-resize-preview" style={frameStyle(bounds)}/>
      ))}
      <Dock items={[
        { id: "liteos", label: "LiteOS", icon: "assets/liteos.png", active: panel === "command", onClick: () => setPanel(panel === "command" ? null : "command") },
        ...dockApps.map((app) => {
          const surface = open.find((item) => item.appId === app.id);
          return {
            ...app,
            running: Boolean(surface),
            active: surface?.id === activeId
              && surfaceWorkspace.get(surface.id) === activeWorkspace,
            onClick: () => launchOrActivate(app),
          };
        }),
        { id: "settings", label: "Settings", icon: "assets/settings.png", active: panel === "system", onClick: () => setPanel(panel === "system" ? null : "system") },
      ]}/>
      {panel === "command" && (
        <CommandCenter
          apps={commandApps}
          onLaunch={(id) => { launch(id); setPanel(null); }}
          onClose={() => setPanel(null)}
          onSystem={() => setPanel("system")}
          onShutdown={shutdown}
        />
      )}
      {panel === "overview" && (
        <WorkspaceOverview
          workspaces={workspaceViews}
          activeWorkspace={activeWorkspace}
          onActivate={activate}
          onSelect={selectWorkspace}
          onClose={() => setPanel(null)}
        />
      )}
      {panel === "system" && (
        <>
          <button className="shell-scrim" aria-label="Close system center" onClick={() => setPanel(null)}/>
          <SystemCenter
            time={time}
            date={date}
            volume={master.percent}
            muted={master.muted}
            onVolume={setVolume}
            onMuted={() => setMuted(!master.muted)}
            onClose={() => setPanel(null)}
          />
        </>
      )}
      <Splash/>
    </div>
  );
}
