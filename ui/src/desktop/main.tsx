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
  WindowSwitcher,
  WorkspaceOverview,
  dockOuterHeight,
} from "../design-system/shell.tsx";
import type { ShellPanel } from "../design-system/shell.tsx";
import { ContextMenu } from "../design-system/context-menu.tsx";
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
const KEY_D = 32;
const KEY_SPACE = 57;
const KEY_LEFT_ALT = 56;
const KEY_F4 = 62;
const KEY_RIGHT_ALT = 100;
const KEY_UP = 103;
const MOD_SHIFT = 1;
const KEY_LEFT = 105;
const KEY_RIGHT = 106;
const KEY_DOWN = 108;
const MOD_CONTROL = 2;
const MOD_ALT = 4;
const MOD_SUPER = 8;
const WORKSPACE_COUNT = 3;
const WORK_AREA_SIDE_MARGIN = 12;
const WORK_AREA_TOP = 56;
const DOCK_BOTTOM_OFFSET = 20;
const DOCK_WORK_AREA_GAP = 12;
const AUTO_HIDE_WORK_AREA_BOTTOM = 12;
const WINDOW_SWITCHER_VISIBLE_LIMIT = 8;
const TOP_EDGE_SNAP_DISTANCE = 4;
const TILE_GAP = 8;

type DesktopMenuState = {
  kind: "desktop";
  x: number;
  y: number;
} | {
  kind: "dock";
  appId: string;
  label: string;
  x: number;
  y: number;
} | {
  kind: "window";
  surfaceId: number;
  x: number;
  y: number;
};

interface ShowDesktopState {
  workspace: number;
  ids: number[];
}

type WindowPlacementKind = "maximized" | "left" | "right";

interface WindowPlacement {
  kind: WindowPlacementKind;
  restore: LiteFrame;
}

const dockApps = [
  { id: "file-manager", label: "Files", icon: "assets/files.png", title: "Files" },
  { id: "terminal", label: "Terminal", icon: "assets/terminal.png", title: "Terminal" },
  { id: "music-player", label: "Music", icon: "assets/music.png", title: "Music" },
  { id: "my-computer", label: "Computer", icon: "assets/package.png", title: "Computer" },
];
// Files and Terminal own independent per-process window state. Restricting New
// Window to them prevents duplicate music playback and redundant system views.
const multiWindowApps = new Set(["file-manager", "terminal"]);

const appIcon = (id: string) => dockApps.find((item) => item.id === id)?.icon ?? "assets/package.png";

const viewport = () => ({ width: window.innerWidth, height: window.innerHeight });

const placementFrame = (kind: WindowPlacementKind, area: Rect): Rect => {
  if (kind === "maximized" || area.width < 2) return area;
  const gap = Math.min(TILE_GAP, area.width - 2);
  const leftWidth = Math.floor((area.width - gap) / 2);
  if (kind === "left") return { ...area, width: leftWidth };
  return {
    x: area.x + leftWidth + gap,
    y: area.y,
    width: area.width - leftWidth - gap,
    height: area.height,
  };
};

const placementFits = (appId: string, kind: WindowPlacementKind, area: Rect) => {
  if (kind === "maximized") return true;
  const candidate = placementFrame(kind, area);
  const minimum = APP_MIN_WINDOWS[appId] ?? DEFAULT_MIN_WINDOW;
  return candidate.width >= minimum.width && candidate.height >= minimum.height;
};

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
  const [showDesktopState, setShowDesktopState] = useState<ShowDesktopState | null>(null);
  const [placements, setPlacements] = useState<Map<number, WindowPlacement>>(() => new Map());
  const [resizePreview, setResizePreview] = useState<Map<number, Rect>>(() => new Map());
  const resizePreviewRef = useRef(resizePreview);
  resizePreviewRef.current = resizePreview;
  const [panel, setPanel] = useState<ShellPanel>(null);
  const panelRef = useRef(panel);
  panelRef.current = panel;
  const [desktopMenu, setDesktopMenu] = useState<DesktopMenuState | null>(null);
  // One Alt hold owns a stable window order. Rebuilding from z-order after
  // every activation would bounce between newly raised windows instead of
  // walking the original switcher sequence; state also drives visible feedback.
  const [windowSwitcher, setWindowSwitcher] = useState<{ ids: number[]; index: number } | null>(null);
  const [clock, setClock] = useState(() => new Date());
  const [master, setMaster] = useState({ percent: 75, muted: false });
  const booted = useRef(false);
  const listedApps = useMemo(() => apps(), []);

  const changePanel = useCallback((next: ShellPanel) => {
    setDesktopMenu(null);
    setWindowSwitcher(null);
    setPanel(next);
  }, []);
  const openDockMenu = useCallback((appId: string, label: string, x: number, y: number) => {
    setPanel(null);
    setWindowSwitcher(null);
    setDesktopMenu({ kind: "dock", appId, label, x, y });
  }, []);

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
    setShowDesktopState((current) => {
      if (!current?.ids.includes(id)) return current;
      const ids = current.ids.filter((candidate) => candidate !== id);
      return ids.length > 0 ? { ...current, ids } : null;
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
    setDesktopMenu(null);
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
      const movedSurface = openRef.current.find((surface) => surface.id === event.surfaceId);
      const restore = movedSurface?.bounds;
      setOpen((current) => applySurfaceMove(current, event.surfaceId, event.x, event.y));
      const displaced = restore && (event.x !== restore.x || event.y !== restore.y);
      const placement = event.y <= desktopArea.y + TOP_EDGE_SNAP_DISTANCE
        ? "maximized"
        : event.x <= desktopArea.x + TOP_EDGE_SNAP_DISTANCE
          ? "left"
          : restore && event.x + restore.width >= desktopArea.x + desktopArea.width - TOP_EDGE_SNAP_DISTANCE
            ? "right"
            : null;
      const placeable = movedSurface && placement
        ? placementFits(movedSurface.appId, placement, desktopArea)
        : false;
      if (restore && displaced && placement && placeable) {
        setPlacements((current) => {
          if (current.has(event.surfaceId)) return current;
          return new Map(current).set(event.surfaceId, { kind: placement, restore });
        });
      }
    }
    if (event.type === "closed") {
      setWindowSwitcher(null);
      setDesktopMenu((current) => current?.kind === "window" && current.surfaceId === event.surfaceId
        ? null
        : current);
      setShowDesktopState((current) => {
        if (!current?.ids.includes(event.surfaceId)) return current;
        const ids = current.ids.filter((id) => id !== event.surfaceId);
        return ids.length > 0 ? { ...current, ids } : null;
      });
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
      setPlacements((current) => {
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

  const restoreShowDesktopWindows = useCallback(() => {
    if (!showDesktopState) return [];
    const liveIds = showDesktopState.ids.filter((id) =>
      openRef.current.some((surface) => surface.id === id)
      && surfaceWorkspaceRef.current.get(id) === showDesktopState.workspace,
    );
    setMinimized((current) => {
      const next = new Set(current);
      for (const id of liveIds) next.delete(id);
      minimizedRef.current = next;
      return next;
    });
    setShowDesktopState(null);
    return liveIds;
  }, [showDesktopState]);

  const showDesktop = useCallback(() => {
    setDesktopMenu(null);
    setWindowSwitcher(null);
    if (showDesktopState) {
      const ids = restoreShowDesktopWindows();
      const target = openRef.current.filter((surface) => ids.includes(surface.id)).at(-1);
      focus(target?.id ?? 0);
      setActiveId(target?.id ?? 0);
      setPanel(null);
      return;
    }
    const ids = openRef.current
      .filter((surface) =>
        surfaceWorkspaceRef.current.get(surface.id) === activeWorkspaceRef.current
        && !minimizedRef.current.has(surface.id),
      )
      .map((surface) => surface.id);
    if (ids.length === 0) return;
    setMinimized((current) => {
      const next = new Set(current);
      for (const id of ids) next.add(id);
      minimizedRef.current = next;
      return next;
    });
    setShowDesktopState({ workspace: activeWorkspaceRef.current, ids });
    focus(0);
    setActiveId(0);
    setPanel(null);
  }, [restoreShowDesktopWindows, showDesktopState]);

  const selectWorkspace = useCallback((workspace: number) => {
    if (workspace < 0 || workspace >= WORKSPACE_COUNT) return;
    if (workspace !== activeWorkspaceRef.current) restoreShowDesktopWindows();
    setDesktopMenu(null);
    setWindowSwitcher(null);
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
  }, [restoreShowDesktopWindows]);

  useEffect(() => {
    setAccelerators([
      { modifiers: MOD_CONTROL, code: KEY_SPACE },
      { modifiers: MOD_CONTROL | MOD_ALT, code: KEY_LEFT },
      { modifiers: MOD_CONTROL | MOD_ALT, code: KEY_RIGHT },
      { modifiers: MOD_ALT, code: KEY_TAB },
      { modifiers: MOD_ALT | MOD_SHIFT, code: KEY_TAB },
      { modifiers: MOD_ALT, code: KEY_F4 },
      { modifiers: MOD_SUPER, code: KEY_D },
      { modifiers: MOD_SUPER, code: KEY_LEFT },
      { modifiers: MOD_SUPER, code: KEY_RIGHT },
      { modifiers: MOD_SUPER, code: KEY_UP },
      { modifiers: MOD_SUPER, code: KEY_DOWN },
    ]);
  }, []);

  const onDesktopKey = (raw: unknown) => {
    const event = raw as LiteKeyEvent;
    if ((event.code === KEY_LEFT_ALT || event.code === KEY_RIGHT_ALT) && event.value === 0) {
      setWindowSwitcher(null);
    }
    if (event.code === KEY_ESC && event.value === 1) {
      setDesktopMenu(null);
      setWindowSwitcher(null);
      setPanel(null);
    }
    if (event.code === KEY_SPACE && event.value === 1 && (event.modifiers & MOD_CONTROL) !== 0) {
      setDesktopMenu(null);
      setWindowSwitcher(null);
      setPanel((current) => current === "command" ? null : "command");
    }
    if (event.code === KEY_F4 && event.value === 1 && (event.modifiers & MOD_ALT) !== 0) {
      setDesktopMenu(null);
      setWindowSwitcher(null);
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
    if (event.code === KEY_D && event.value === 1 && event.modifiers === MOD_SUPER) {
      showDesktop();
    }
    if (event.value === 1 && event.modifiers === MOD_SUPER) {
      const id = activeIdRef.current;
      if (!id) return;
      if (event.code === KEY_LEFT) placeWindow(id, "left");
      else if (event.code === KEY_RIGHT) placeWindow(id, "right");
      else if (event.code === KEY_UP) placeWindow(id, "maximized");
      else if (event.code === KEY_DOWN && placements.has(id)) togglePlacement(id);
    }
    if (event.code === KEY_TAB && event.value === 1 && (event.modifiers & MOD_ALT) !== 0) {
      const available = openRef.current
        .filter((surface) => surfaceWorkspaceRef.current.get(surface.id) === activeWorkspaceRef.current)
        .map((surface) => surface.id);
      if (available.length === 0) return;
      const current = windowSwitcher;
      const sameCycle = current
        && current.ids.length === available.length
        && current.ids.every((id) => available.includes(id));
      const cycle = sameCycle && current
        ? current
        : {
          ids: available,
          index: Math.max(0, available.indexOf(activeIdRef.current)),
        };
      const direction = (event.modifiers & MOD_SHIFT) !== 0 ? 1 : -1;
      const index = (cycle.index + direction + cycle.ids.length) % cycle.ids.length;
      setWindowSwitcher({ ids: cycle.ids, index });
      activate(cycle.ids[index]);
    }
  };

  const launchOrActivate = useCallback((appId: string) => {
    const existing = openRef.current.filter((surface) => surface.appId === appId).at(-1);
    if (existing) {
      activate(existing.id);
      return;
    }
    launch(appId);
    setDesktopMenu(null);
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

  const togglePlacement = useCallback((id: number) => {
    const surface = openRef.current.find((item) => item.id === id);
    if (!surface) return;
    const placement = placements.get(id);
    if (placement) {
      const bounds = fitSurfaceFrame(placement.restore, desktopArea);
      move(id, bounds.x, bounds.y);
      setOpen((current) => current.map((surface) =>
        surface.id === id ? { ...surface, bounds } : surface,
      ));
      setPlacements((current) => {
        const next = new Map(current);
        next.delete(id);
        return next;
      });
      activate(id);
      return;
    }
    setPlacements((current) => new Map(current).set(id, {
      kind: "maximized",
      restore: surface.bounds,
    }));
    activate(id);
  }, [activate, desktopArea, placements]);

  const placeWindow = useCallback((id: number, kind: WindowPlacementKind) => {
    const surface = openRef.current.find((item) => item.id === id);
    if (!surface || !placementFits(surface.appId, kind, desktopArea)) return;
    setPlacements((current) => {
      const placement = current.get(id);
      if (placement?.kind === kind) return current;
      return new Map(current).set(id, {
        kind,
        restore: placement?.restore ?? surface.bounds,
      });
    });
    activate(id);
  }, [activate, desktopArea]);

  const beginWindowMove = useCallback((id: number, serial: number) => {
    if (placements.has(id)) return;
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
  }, [desktopArea, placements]);

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
    setShowDesktopState((current) => {
      if (!current?.ids.includes(id)) return current;
      const ids = current.ids.filter((candidate) => candidate !== id);
      return ids.length > 0 ? { ...current, ids } : null;
    });
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
  // A session may own 32 surfaces. Limiting the visible neighborhood keeps the
  // transient panel on-screen; without it, later selections would be clipped.
  const switcherVisibleIds = windowSwitcher && windowSwitcher.ids.length > WINDOW_SWITCHER_VISIBLE_LIMIT
    ? Array.from({ length: WINDOW_SWITCHER_VISIBLE_LIMIT }, (_, offset) => {
      const start = windowSwitcher.index - Math.floor(WINDOW_SWITCHER_VISIBLE_LIMIT / 2);
      return windowSwitcher.ids[(start + offset + windowSwitcher.ids.length) % windowSwitcher.ids.length];
    })
    : windowSwitcher?.ids ?? [];
  const switcherWindows = windowSwitcher
    ? switcherVisibleIds
      .map((id) => open.find((surface) => surface.id === id))
      .filter((surface): surface is LiteSurface => surface !== undefined)
      .map((surface) => ({ ...surface, minimized: minimized.has(surface.id) }))
    : [];
  const switcherSelectedId = windowSwitcher?.ids[windowSwitcher.index] ?? 0;
  const dockMenuSurfaces = desktopMenu?.kind === "dock"
    ? open.filter((surface) => surface.appId === desktopMenu.appId)
    : [];
  const dockMenuTarget = dockMenuSurfaces.at(-1);
  const dockMenuPlacement = dockMenuTarget ? placements.get(dockMenuTarget.id) : undefined;
  const windowMenuTarget = desktopMenu?.kind === "window"
    ? open.find((surface) => surface.id === desktopMenu.surfaceId)
    : undefined;
  const windowMenuPlacement = windowMenuTarget ? placements.get(windowMenuTarget.id) : undefined;
  const windowMenuWorkspace = windowMenuTarget ? surfaceWorkspace.get(windowMenuTarget.id) : undefined;
  const time = `${String(clock.getHours()).padStart(2, "0")}:${String(clock.getMinutes()).padStart(2, "0")}`;
  const weekdays = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
  const months = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
  const date = `${weekdays[clock.getDay()]}, ${months[clock.getMonth()]} ${clock.getDate()}`;

  return (
    <div id="desktop" className="aurora-root" onKeyDown={onDesktopKey}>
      <div
        className="desktop-background-hit"
        onContextMenu={(rawEvent) => {
          const event = rawEvent as unknown as LitePointerEvent;
          event.stopPropagation();
          setPanel(null);
          setWindowSwitcher(null);
          setDesktopMenu({ kind: "desktop", x: event.x, y: event.y });
        }}
      />
      <TopBar
        panel={panel}
        time={time}
        volume={master.percent}
        muted={master.muted}
        activeWorkspace={activeWorkspace}
        workspaceCount={WORKSPACE_COUNT}
        onPanel={changePanel}
      />
      {visible.map((surface) => {
        const placement = placements.get(surface.id);
        const bounds = placement ? placementFrame(placement.kind, desktopArea) : surface.bounds;
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
            onTogglePlacement={togglePlacement}
            onOpenSystemMenu={(surfaceId, x, y) => {
              activate(surfaceId);
              setWindowSwitcher(null);
              setDesktopMenu({ kind: "window", surfaceId, x, y });
            }}
            placed={placement !== undefined}
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
      {windowSwitcher && switcherWindows.length > 0 && (
        <WindowSwitcher
          windows={switcherWindows}
          selectedId={switcherSelectedId}
          position={windowSwitcher.index + 1}
          total={windowSwitcher.ids.length}
        />
      )}
      <Dock
        iconSize={dockIconSize}
        autoHide={dockAutoHide}
        items={[
          {
            id: "liteos",
            label: "LiteOS",
            icon: "assets/liteos.png",
            active: panel === "command",
            onClick: () => changePanel(panel === "command" ? null : "command"),
            onContextMenu: (x: number, y: number) => openDockMenu("liteos", "LiteOS", x, y),
          },
          ...dockApps.map((app) => {
            const appSurfaces = open.filter((surface) => surface.appId === app.id);
            return {
              ...app,
              running: appSurfaces.length > 0,
              active: appSurfaces.some((surface) =>
                surface.id === activeId
                && surfaceWorkspace.get(surface.id) === activeWorkspace),
              onClick: () => launchOrActivate(app.id),
              onContextMenu: (x: number, y: number) => openDockMenu(app.id, app.label, x, y),
            };
          }),
          {
            id: "settings",
            label: "Settings",
            icon: "assets/settings.png",
            active: panel === "system",
            onClick: () => changePanel(panel === "system" ? null : "system"),
            onContextMenu: (x: number, y: number) => openDockMenu("settings", "Settings", x, y),
          },
        ]}
      />
      {panel === "command" && (
        <CommandCenter
          apps={commandApps}
          activeWorkspace={activeWorkspace}
          onLaunch={launchOrActivate}
          onClose={() => changePanel(null)}
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
          onClose={() => changePanel(null)}
        />
      )}
      {panel === "system" && (
        <>
          <button className="shell-scrim" aria-label="Close system center" onClick={() => changePanel(null)}/>
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
            onClose={() => changePanel(null)}
          />
        </>
      )}
      {desktopMenu && (
        <>
          <button
            className="desktop-menu-scrim"
            aria-label="Close desktop menu"
            onClick={() => setDesktopMenu(null)}
            onContextMenu={(rawEvent) => {
              const event = rawEvent as unknown as LitePointerEvent;
              event.stopPropagation();
              setDesktopMenu(null);
            }}
          />
          <ContextMenu
            x={desktopMenu.x}
            y={desktopMenu.y}
            items={desktopMenu.kind === "desktop"
              ? [
                { id: "command", label: "Open Command Center", onSelect: () => changePanel("command") },
                { id: "apps-separator", label: "", separator: true },
                { id: "files", label: "Open Files", onSelect: () => launchOrActivate("file-manager") },
                {
                  id: "terminal",
                  label: "New Terminal Window",
                  onSelect: () => {
                    launch("terminal");
                    changePanel(null);
                  },
                },
                { id: "desktop-separator", label: "", separator: true },
                {
                  id: "show-desktop",
                  label: showDesktopState ? "Restore Desktop Windows" : "Show Desktop",
                  disabled: !showDesktopState && visible.length === 0,
                  onSelect: showDesktop,
                },
                { id: "workspaces", label: "Workspace Overview", onSelect: () => changePanel("overview") },
                { id: "settings", label: "System Center", onSelect: () => changePanel("system") },
              ]
              : desktopMenu.kind === "window"
                ? windowMenuTarget ? [
                  {
                    id: "minimize-window",
                    label: "Minimize",
                    onSelect: () => minimizeWindow(windowMenuTarget.id),
                  },
                  { id: "placement-separator", label: "", separator: true },
                  {
                    id: "maximize-window",
                    label: "Maximize",
                    disabled: windowMenuPlacement?.kind === "maximized",
                    onSelect: () => placeWindow(windowMenuTarget.id, "maximized"),
                  },
                  {
                    id: "tile-window-left",
                    label: "Tile Left",
                    disabled: windowMenuPlacement?.kind === "left"
                      || !placementFits(windowMenuTarget.appId, "left", desktopArea),
                    onSelect: () => placeWindow(windowMenuTarget.id, "left"),
                  },
                  {
                    id: "tile-window-right",
                    label: "Tile Right",
                    disabled: windowMenuPlacement?.kind === "right"
                      || !placementFits(windowMenuTarget.appId, "right", desktopArea),
                    onSelect: () => placeWindow(windowMenuTarget.id, "right"),
                  },
                  {
                    id: "restore-window",
                    label: "Restore",
                    disabled: !windowMenuPlacement,
                    onSelect: () => togglePlacement(windowMenuTarget.id),
                  },
                  { id: "workspace-separator", label: "", separator: true },
                  ...Array.from({ length: WORKSPACE_COUNT }, (_, workspace) => workspace)
                    .filter((workspace) => workspace !== windowMenuWorkspace)
                    .map((workspace) => ({
                      id: `move-workspace-${workspace}`,
                      label: `Move to Workspace ${workspace + 1}`,
                      onSelect: () => moveWindowToWorkspace(windowMenuTarget.id, workspace),
                    })),
                  { id: "close-separator", label: "", separator: true },
                  {
                    id: "close-window",
                    label: "Close",
                    onSelect: () => closeWindow(windowMenuTarget.id),
                  },
                ] : []
                : desktopMenu.appId === "liteos"
                  ? [{ id: "open-command", label: "Open Command Center", onSelect: () => changePanel("command") }]
                  : desktopMenu.appId === "settings"
                    ? [{ id: "open-settings", label: "Open System Center", onSelect: () => changePanel("system") }]
                    : [
                    {
                      id: "open-app",
                      label: open.some((surface) => surface.appId === desktopMenu.appId)
                        ? `Show ${desktopMenu.label}`
                        : `Open ${desktopMenu.label}`,
                      onSelect: () => launchOrActivate(desktopMenu.appId),
                    },
                    ...(multiWindowApps.has(desktopMenu.appId)
                      && open.some((surface) => surface.appId === desktopMenu.appId)
                      ? [{
                        id: "new-window",
                        label: "New Window",
                        onSelect: () => {
                          launch(desktopMenu.appId);
                          changePanel(null);
                        },
                      }]
                      : []),
                    ...(dockMenuSurfaces.length > 1 ? [
                      { id: "windows-separator", label: "", separator: true },
                      ...dockMenuSurfaces.map((surface, index) => {
                        const workspace = surfaceWorkspace.get(surface.id);
                        return {
                          id: `show-window-${surface.id}`,
                          label: `${index + 1}. ${surface.title}${workspace === undefined ? "" : ` · W${workspace + 1}`}${minimized.has(surface.id) ? " · Minimized" : ""}`,
                          onSelect: () => activate(surface.id),
                        };
                      }),
                    ] : []),
                    ...(dockMenuTarget ? [
                      { id: "window-separator", label: "", separator: true },
                      {
                        id: "maximize-window",
                        label: "Maximize Window",
                        disabled: dockMenuPlacement?.kind === "maximized",
                        onSelect: () => placeWindow(dockMenuTarget.id, "maximized"),
                      },
                      {
                        id: "tile-window-left",
                        label: "Tile Window Left",
                        disabled: dockMenuPlacement?.kind === "left"
                          || !placementFits(desktopMenu.appId, "left", desktopArea),
                        onSelect: () => placeWindow(dockMenuTarget.id, "left"),
                      },
                      {
                        id: "tile-window-right",
                        label: "Tile Window Right",
                        disabled: dockMenuPlacement?.kind === "right"
                          || !placementFits(desktopMenu.appId, "right", desktopArea),
                        onSelect: () => placeWindow(dockMenuTarget.id, "right"),
                      },
                      {
                        id: "restore-window",
                        label: "Restore Window",
                        disabled: !dockMenuPlacement,
                        onSelect: () => togglePlacement(dockMenuTarget.id),
                      },
                      { id: "close-separator", label: "", separator: true },
                      {
                        id: "close-app",
                        label: "Close All Windows",
                        onSelect: () => {
                          for (const surface of open.filter((surface) => surface.appId === desktopMenu.appId)) {
                            closeWindow(surface.id);
                          }
                        },
                      },
                    ] : []),
                    ]}
            onClose={() => setDesktopMenu(null)}
          />
        </>
      )}
      <Splash/>
    </div>
  );
}
