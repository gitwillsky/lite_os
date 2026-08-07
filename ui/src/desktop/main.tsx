import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { apps, launch } from "lite:apps";
import { getState, setMuted, setVolume, subscribe } from "lite:audio-system";
import {
  beginMove,
  close,
  configure,
  focus,
  move,
  restart,
  setAccelerators,
  shutdown,
  surfaces,
} from "lite:desktop";
import { Window } from "../design-system/window.tsx";
import {
  DOCK_DEFAULT_ICON_SIZE,
  Dock,
  CommandCenter,
  SystemCenter,
  TopBar,
  WorkspaceOverview,
  dockOuterHeight,
} from "../design-system/shell.tsx";
import type { ShellPanel } from "../design-system/shell.tsx";
import { constrainResize, frameStyle } from "../design-system/window-geometry.ts";
import type { Rect, ResizeCandidate } from "../design-system/window-geometry.ts";
import { applySurfaceMove, fitSurfaceFrame, reconcileSurfaces } from "./surface-state.ts";
import { Splash } from "./splash.tsx";

const DEFAULT_MIN_WINDOW = { width: 360, height: 240 };
const APP_MIN_WINDOWS: Record<string, { width: number; height: number }> = {
  "file-manager": { width: 760, height: 460 },
  "my-computer": { width: 700, height: 440 },
  "music-player": { width: 600, height: 420 },
  terminal: { width: 360, height: 220 },
};
const KEY_ESC = 1;
const KEY_TAB = 15;
const KEY_SPACE = 57;
const KEY_LEFT_ALT = 56;
const KEY_F4 = 62;
const KEY_RIGHT_ALT = 100;
const MOD_SHIFT = 1;
const KEY_LEFT = 105;
const KEY_RIGHT = 106;
const MOD_CONTROL = 2;
const MOD_ALT = 4;
const WORKSPACE_COUNT = 3;
const WORK_AREA_SIDE_MARGIN = 12;
const WORK_AREA_TOP = 56;
const DOCK_BOTTOM_OFFSET = 20;
const DOCK_WORK_AREA_GAP = 12;
const AUTO_HIDE_WORK_AREA_BOTTOM = 12;

const dockApps = [
  { id: "file-manager", label: "Files", icon: "assets/files.png", title: "Files" },
  { id: "terminal", label: "Terminal", icon: "assets/terminal.png", title: "Terminal" },
  { id: "music-player", label: "Music", icon: "assets/music.png", title: "Music" },
  { id: "my-computer", label: "Computer", icon: "assets/package.png", title: "Computer" },
];

const appIcon = (id: string) => dockApps.find((item) => item.id === id)?.icon ?? "assets/package.png";

const viewport = () => ({ width: window.innerWidth, height: window.innerHeight });

const workArea = (screen: { width: number; height: number }, bottomInset: number): Rect => {
  const x = Math.min(WORK_AREA_SIDE_MARGIN, Math.max(0, screen.width - 1));
  const y = Math.min(WORK_AREA_TOP, Math.max(0, screen.height - 55));
  return {
    x,
    y,
    width: Math.max(
      1,
      screen.width - x - Math.min(WORK_AREA_SIDE_MARGIN, screen.width - x - 1),
    ),
    height: Math.max(
      55,
      screen.height - y - Math.min(bottomInset, screen.height - y - 55),
    ),
  };
};

export default function Desktop() {
  const [screen, setScreen] = useState(viewport);
  const [dockIconSize, setDockIconSize] = useState(DOCK_DEFAULT_ICON_SIZE);
  const [dockAutoHide, setDockAutoHide] = useState(false);
  // A visible Dock owns its full chrome plus breathing room. Auto-hide releases
  // that space; without the remaining inset, bottom resize targets touch the output edge.
  const dockBottomInset = dockAutoHide
    ? AUTO_HIDE_WORK_AREA_BOTTOM
    : dockOuterHeight(dockIconSize) + DOCK_BOTTOM_OFFSET + DOCK_WORK_AREA_GAP;
  const desktopArea = useMemo(
    () => workArea(screen, dockBottomInset),
    [dockBottomInset, screen],
  );
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
  const panelRef = useRef(panel);
  panelRef.current = panel;
  // One Alt hold owns a stable window order. Rebuilding from z-order after
  // every activation would bounce between newly raised windows instead of
  // walking the original switcher sequence.
  const switcher = useRef<{ ids: number[]; index: number } | null>(null);
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
    if (panel !== null) {
      focus(0);
      return;
    }
    const active = openRef.current.find((surface) => surface.id === activeIdRef.current);
    const visible = active
      && surfaceWorkspaceRef.current.get(active.id) === activeWorkspaceRef.current
      && !minimizedRef.current.has(active.id);
    focus(visible ? active.id : 0);
  }, [panel]);

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
      // JS `open` is the sole focus authority — the native registry no longer
      // self-focuses a new surface. Record it as active, but keep compositor
      // keyboard focus on desktop while a shell panel owns interaction.
      if (panelRef.current === null) focus(event.surface.id);
      setActiveId(event.surface.id);
    }
    if (event.type === "activated"
      && panelRef.current === null
      && !minimizedRef.current.has(event.surfaceId)) {
      // A delayed activation from an earlier launch must not steal keyboard
      // focus from a shell panel that has since opened.
      synchronizeActivation(event.surfaceId);
    }
    if (event.type === "moved") {
      setOpen((current) => applySurfaceMove(current, event.surfaceId, event.x, event.y));
    }
    if (event.type === "closed") {
      switcher.current = null;
      // JS `open` is the sole focus authority: the native registry cleared its
      // keyboard target to the desktop when the surface closed, so when the
      // closed window was active we pick its replacement here (last visible in
      // the active workspace, same policy as minimize) and drive `focus()`.
      // Without this the compositor would route keys to nothing until the next
      // click.
      setActiveId((current) => {
        if (current !== event.surfaceId) return current;
        const fallback = openRef.current
          .filter((surface) =>
            surface.id !== event.surfaceId
            && surfaceWorkspaceRef.current.get(surface.id) === activeWorkspaceRef.current
            && !minimizedRef.current.has(surface.id),
          )
          .at(-1);
        const next = fallback?.id ?? 0;
        focus(next);
        return next;
      });
      setMinimized((current) => {
        const next = new Set(current);
        next.delete(event.surfaceId);
        return next;
      });
      setMaximized((current) => {
        const next = new Map(current);
        next.delete(event.surfaceId);
        return next;
      });
      setResizePreview((current) => {
        const next = new Map(current);
        next.delete(event.surfaceId);
        return next;
      });
      setSurfaceWorkspace((current) => {
        const next = new Map(current);
        next.delete(event.surfaceId);
        return next;
      });
    }
  }), [desktopArea, synchronizeActivation]);

  const selectWorkspace = useCallback((workspace: number) => {
    if (workspace < 0 || workspace >= WORKSPACE_COUNT) return;
    switcher.current = null;
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

  useEffect(() => {
    setAccelerators([
      { modifiers: MOD_CONTROL, code: KEY_SPACE },
      { modifiers: MOD_CONTROL | MOD_ALT, code: KEY_LEFT },
      { modifiers: MOD_CONTROL | MOD_ALT, code: KEY_RIGHT },
      { modifiers: MOD_ALT, code: KEY_TAB },
      { modifiers: MOD_ALT | MOD_SHIFT, code: KEY_TAB },
      { modifiers: MOD_ALT, code: KEY_F4 },
    ]);
  }, []);

  const onDesktopKey = (raw: unknown) => {
    const event = raw as LiteKeyEvent;
    if ((event.code === KEY_LEFT_ALT || event.code === KEY_RIGHT_ALT) && event.value === 0) {
      switcher.current = null;
    }
    if (event.code === KEY_ESC && event.value === 1) {
      setPanel(null);
    }
    if (event.code === KEY_SPACE && event.value === 1 && (event.modifiers & MOD_CONTROL) !== 0) {
      setPanel((current) => current === "command" ? null : "command");
    }
    if (event.code === KEY_F4 && event.value === 1 && (event.modifiers & MOD_ALT) !== 0) {
      if (panel !== null) {
        setPanel(null);
        return;
      }
      const id = activeIdRef.current;
      if (id) closeWindow(id);
    }
    if (event.value === 1 && event.modifiers === (MOD_CONTROL | MOD_ALT)) {
      if (event.code === KEY_LEFT) {
        selectWorkspace((activeWorkspaceRef.current + WORKSPACE_COUNT - 1) % WORKSPACE_COUNT);
      } else if (event.code === KEY_RIGHT) {
        selectWorkspace((activeWorkspaceRef.current + 1) % WORKSPACE_COUNT);
      }
    }
    if (event.code === KEY_TAB && event.value === 1 && (event.modifiers & MOD_ALT) !== 0) {
      const available = openRef.current
        .filter((surface) => surfaceWorkspaceRef.current.get(surface.id) === activeWorkspaceRef.current)
        .map((surface) => surface.id);
      if (available.length === 0) return;
      const current = switcher.current;
      const sameCycle = current
        && current.ids.length === available.length
        && current.ids.every((id) => available.includes(id));
      if (!sameCycle) {
        switcher.current = {
          ids: available,
          index: Math.max(0, available.indexOf(activeIdRef.current)),
        };
      }
      const cycle = switcher.current!;
      const direction = (event.modifiers & MOD_SHIFT) !== 0 ? 1 : -1;
      cycle.index = (cycle.index + direction + cycle.ids.length) % cycle.ids.length;
      activate(cycle.ids[cycle.index]);
    }
  };

  const launchOrActivate = useCallback((appId: string) => {
    const existing = openRef.current.filter((surface) => surface.appId === appId).at(-1);
    if (existing) {
      activate(existing.id);
      return;
    }
    launch(appId);
    setPanel(null);
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
    // Each app has a distinct smallest usable layout. A single tiny frame
    // limit lets explorer sidebars and player transport controls overlap;
    // the work-area clamp still wins on genuinely small displays.
    const surface = openRef.current.find((item) => item.id === id);
    const minimum = surface ? APP_MIN_WINDOWS[surface.appId] ?? DEFAULT_MIN_WINDOW : DEFAULT_MIN_WINDOW;
    const bounds = constrainResize(
      candidate,
      desktopArea,
      Math.min(minimum.width, desktopArea.width),
      Math.min(minimum.height, desktopArea.height),
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
    running: open.some((surface) => surface.appId === app.id),
  }));
  const moveWindowToWorkspace = useCallback((id: number, workspace: number) => {
    if (workspace < 0 || workspace >= WORKSPACE_COUNT) return;
    const previousWorkspace = surfaceWorkspaceRef.current.get(id);
    if (previousWorkspace === undefined || previousWorkspace === workspace) return;
    const nextWorkspaces = new Map(surfaceWorkspaceRef.current).set(id, workspace);
    surfaceWorkspaceRef.current = nextWorkspaces;
    setSurfaceWorkspace(nextWorkspaces);
    if (id === activeIdRef.current && previousWorkspace === activeWorkspaceRef.current) {
      const fallback = openRef.current
        .filter((surface) =>
          surface.id !== id
          && nextWorkspaces.get(surface.id) === activeWorkspaceRef.current
          && !minimizedRef.current.has(surface.id),
        )
        .at(-1);
      const next = fallback?.id ?? 0;
      focus(next);
      setActiveId(next);
    }
  }, []);
  const visible = open.filter((surface) =>
    surfaceWorkspace.get(surface.id) === activeWorkspace && !minimized.has(surface.id),
  );
  const workspaceViews = Array.from({ length: WORKSPACE_COUNT }, (_, id) => ({
    id,
    windows: open
      .filter((surface) => surfaceWorkspace.get(surface.id) === id)
      .map((surface) => ({ ...surface, minimized: minimized.has(surface.id) })),
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
        volume={master.percent}
        muted={master.muted}
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
      <Dock
        iconSize={dockIconSize}
        autoHide={dockAutoHide}
        items={[
          { id: "liteos", label: "LiteOS", icon: "assets/liteos.png", active: panel === "command", onClick: () => setPanel(panel === "command" ? null : "command") },
          ...dockApps.map((app) => {
            const appSurfaces = open.filter((surface) => surface.appId === app.id);
            return {
              ...app,
              running: appSurfaces.length > 0,
              active: appSurfaces.some((surface) =>
                surface.id === activeId
                && surfaceWorkspace.get(surface.id) === activeWorkspace),
              onClick: () => launchOrActivate(app.id),
            };
          }),
          { id: "settings", label: "Settings", icon: "assets/settings.png", active: panel === "system", onClick: () => setPanel(panel === "system" ? null : "system") },
        ]}
      />
      {panel === "command" && (
        <CommandCenter
          apps={commandApps}
          activeWorkspace={activeWorkspace}
          onLaunch={launchOrActivate}
          onClose={() => setPanel(null)}
          onRestart={restart}
          onShutdown={shutdown}
        />
      )}
      {panel === "overview" && (
        <WorkspaceOverview
          workspaces={workspaceViews}
          activeWorkspace={activeWorkspace}
          onActivate={activate}
          onSelect={selectWorkspace}
          onMoveWindow={moveWindowToWorkspace}
          onCloseWindow={closeWindow}
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
            activeWorkspace={activeWorkspace}
            openWindows={open.length}
            dockIconSize={dockIconSize}
            dockAutoHide={dockAutoHide}
            onVolume={setVolume}
            onMuted={() => setMuted(!master.muted)}
            onDockIconSize={setDockIconSize}
            onDockAutoHide={() => setDockAutoHide(!dockAutoHide)}
          />
        </>
      )}
      <Splash/>
    </div>
  );
}
