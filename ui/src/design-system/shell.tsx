import React, { useMemo, useState } from "react";
import { RangeInput, SystemIcon } from "./controls.tsx";
import { CloseGlyph } from "./window.tsx";

const KEY_ESC = 1;
const KEY_ENTER = 28;
const stopPanelPointer = (rawEvent: unknown) =>
  (rawEvent as LitePointerEvent).stopPropagation();

export type ShellPanel = "command" | "overview" | "system" | null;

interface TopBarProps {
  panel: ShellPanel;
  time: string;
  volume: number;
  muted: boolean;
  activeWorkspace: number;
  workspaceCount: number;
  onPanel: (panel: ShellPanel) => void;
}

/** System top bar: global entry, workspace state and system status. */
export function TopBar({
  panel,
  time,
  volume,
  muted,
  activeWorkspace,
  workspaceCount,
  onPanel,
}: TopBarProps) {
  const toggle = (next: Exclude<ShellPanel, null>) => onPanel(panel === next ? null : next);
  return (
    <div className="topbar">
      <button
        className="topbar__brand"
        aria-label="Open applications"
        aria-pressed={panel === "command"}
        onClick={() => toggle("command")}
      >
        <span className="lite-mark"><span/></span>
        <span className="control-label">LiteOS</span>
      </button>
      <button
        className="workspace-switcher"
        aria-label={`Open workspace overview; Workspace ${activeWorkspace + 1} active`}
        aria-pressed={panel === "overview"}
        onClick={() => toggle("overview")}
      >
        <span className="control-label">Workspace {activeWorkspace + 1}</span>
        <span className="workspace-dots">
          {Array.from({ length: workspaceCount }, (_, index) => (
            <span
              key={index}
              className={`workspace-dot${index === activeWorkspace ? " workspace-dot--active" : ""}`}
            />
          ))}
        </span>
      </button>
      <button
        className="topbar__status"
        aria-label={`Open system controls; ${muted ? "muted" : `volume ${volume} percent`}; ${time}`}
        aria-pressed={panel === "system"}
        onClick={() => toggle("system")}
      >
        <img className="status-glyph" src="assets/volume.png" alt=""/>
        <span className="topbar__volume control-label">{muted ? "Muted" : `${volume}%`}</span>
        <span className="topbar__divider"/>
        <span className="control-label">{time}</span>
      </button>
    </div>
  );
}

interface DockItem {
  id: string;
  label: string;
  icon: string;
  running?: boolean;
  active?: boolean;
  onClick: () => void;
}

/** Pinned/running application switcher. Global panels live in the top bar. */
export function Dock({ items }: { items: DockItem[] }) {
  return (
    <div className="dock">
      {items.map((item) => (
        <button
          key={item.id}
          className={`dock-item${item.active ? " dock-item--active" : ""}`}
          aria-label={item.label}
          aria-pressed={item.active}
          onClick={item.onClick}
        >
          <img src={item.icon} alt=""/>
          {(item.running || item.active) && <span className="dock-item__running"/>}
          <span className="dock-item__label">{item.label}</span>
        </button>
      ))}
    </div>
  );
}

interface CommandApp {
  id: string;
  name: string;
  icon: string;
  running: boolean;
}

/** Search-first global command and application launcher. */
export function CommandCenter({
  apps,
  activeWorkspace,
  onLaunch,
  onClose,
  onRestart,
  onShutdown,
}: {
  apps: CommandApp[];
  activeWorkspace: number;
  onLaunch: (id: string) => void;
  onClose: () => void;
  onRestart: () => void;
  onShutdown: () => void;
}) {
  const [query, setQuery] = useState("");
  const [sessionAction, setSessionAction] = useState<"restart" | "shutdown" | null>(null);
  const matches = useMemo(() => {
    const normalized = query.trim().toLocaleLowerCase();
    return normalized
      ? apps.filter((app) =>
        app.name.toLocaleLowerCase().includes(normalized)
        || app.id.toLocaleLowerCase().includes(normalized),
      )
      : apps;
  }, [apps, query]);
  return (
    <>
      <button className="shell-scrim" aria-label="Close command center" onClick={onClose}/>
      <div
        className="command-center"
        data-lite-focus-scope={true}
        onPointerDown={stopPanelPointer}
        onKeyDown={(rawEvent) => {
          const event = rawEvent as unknown as LiteKeyEvent;
          if (event.value !== 1) return;
          if (event.code === KEY_ESC) {
            event.stopPropagation();
            if (sessionAction) setSessionAction(null);
            else if (query) setQuery("");
            else onClose();
          } else if (event.code === KEY_ENTER) {
            event.stopPropagation();
            if (sessionAction === "restart") onRestart();
            else if (sessionAction === "shutdown") onShutdown();
            else if (matches[0]) onLaunch(matches[0].id);
          }
        }}
      >
        <div className="cc-sidebar">
          <div className="cc-sidebar__brand">
            <span className="lite-mark"><span/></span>
            <div className="cc-sidebar__brand-text">
              <span>LiteOS</span>
              <span className="cc-sidebar__sub">Workspace {activeWorkspace + 1}</span>
            </div>
          </div>
          <button
            className="cc-sidebar__all"
            onClick={() => {
              setQuery("");
              setSessionAction(null);
            }}
          >
            <img src="assets/all-apps.png" alt=""/>
            <span className="control-label">All apps</span>
          </button>
        </div>
        <div className="cc-main">
          <div className="command-search">
            <SystemIcon name="search" className="search-glyph"/>
            <input
              autoFocus={true}
              value={query}
              placeholder="Search applications"
              onInput={(event) => setQuery((event as unknown as { value: string }).value)}
            />
            <span className="key-hint"><span className="control-label">Ctrl</span></span>
            <span className="key-hint"><span className="control-label">Space</span></span>
          </div>
          <div className="command-section">
            <div className="command-section__head">
              <span className="section-label">APPLICATIONS</span>
              <span className="command-section__count">{matches.length} available</span>
            </div>
            <div className="command-grid">
              {matches.map((app) => (
                <button key={app.id} className="command-app" onClick={() => onLaunch(app.id)}>
                  <img src={app.icon} alt=""/>
                  <span className="control-label">{app.name}</span>
                  <span className={`command-app__state control-label${app.running ? " command-app__state--running" : ""}`}>
                    {app.running ? "Running" : "Open"}
                  </span>
                </button>
              ))}
              {matches.length === 0 && <span className="command-empty">No matching applications</span>}
            </div>
          </div>
          <div className="cc-footer">
            {sessionAction === null ? (
              <>
                <span className="cc-footer__hint">
                  {matches.length > 0 ? (
                    <>
                      <span className="key-hint"><span className="control-label">Enter</span></span>
                      <span>Open first result</span>
                    </>
                  ) : (
                    <span>Adjust the search to find an application</span>
                  )}
                </span>
                <div className="cc-session-actions">
                  <button className="cc-session-action" onClick={() => setSessionAction("restart")}>
                    <img src="assets/restart.png" alt=""/>
                    <span className="control-label">Restart</span>
                  </button>
                  <button className="cc-session-action cc-session-action--danger" onClick={() => setSessionAction("shutdown")}>
                    <img src="assets/power.png" alt=""/>
                    <span className="control-label">Power off</span>
                  </button>
                </div>
              </>
            ) : (
              <>
                <span className="cc-session-confirmation">
                  {sessionAction === "restart" ? "Restart LiteOS now?" : "Power off LiteOS now?"}
                </span>
                <div className="cc-session-actions">
                  <button className="cc-session-action" onClick={() => setSessionAction(null)}>
                    <span className="control-label">Cancel</span>
                  </button>
                  <button
                    className="cc-session-action cc-session-action--danger"
                    onClick={sessionAction === "restart" ? onRestart : onShutdown}
                  >
                    <span className="control-label">
                      {sessionAction === "restart" ? "Restart now" : "Power off now"}
                    </span>
                  </button>
                </div>
              </>
            )}
          </div>
        </div>
      </div>
    </>
  );
}

interface WorkspaceView {
  id: number;
  windows: Array<LiteSurface & { minimized: boolean }>;
}

/** Spatial overview of running windows and available workspaces. */
export function WorkspaceOverview({
  workspaces,
  activeWorkspace,
  onActivate,
  onSelect,
  onMoveWindow,
  onCloseWindow,
  onClose,
}: {
  workspaces: WorkspaceView[];
  activeWorkspace: number;
  onActivate: (id: number) => void;
  onSelect: (id: number) => void;
  onMoveWindow: (id: number, workspace: number) => void;
  onCloseWindow: (id: number) => void;
  onClose: () => void;
}) {
  const activeWindows = workspaces.find((workspace) => workspace.id === activeWorkspace)?.windows ?? [];
  return (
    <>
      <button className="shell-scrim overview__scrim" aria-label="Close workspace overview" onClick={onClose}/>
      <div className="overview">
        <div className="overview__panel" data-lite-focus-scope={true} onPointerDown={stopPanelPointer}>
          <div className="overview__header">
            <div className="overview__title-group">
              <span className="overview__mark"><img src="assets/all-apps.png" alt=""/></span>
              <span className="overview__title">Workspaces</span>
            </div>
            <span className="overview__summary">{workspaces.length} workspaces</span>
          </div>
          <div className="workspace-list">
            {workspaces.map((workspace) => (
              <button
                key={workspace.id}
                className={`workspace-card${workspace.id === activeWorkspace ? " workspace-card--active" : ""}`}
                aria-label={`Switch to Workspace ${workspace.id + 1}; ${workspace.windows.length === 0 ? "empty" : `${workspace.windows.length} ${workspace.windows.length === 1 ? "window" : "windows"}`}`}
                aria-pressed={workspace.id === activeWorkspace}
                onClick={() => onSelect(workspace.id)}
              >
                {workspace.windows.length > 0 ? (
                  <div className="workspace-card__preview">
                    {workspace.windows.slice(0, 2).map((window, index) => (
                      <span
                        key={window.id}
                        className={`mini-window mini-window--${index}${window.minimized ? " mini-window--minimized" : ""}`}
                      >
                        <img src={window.icon} alt=""/>
                        <span>{window.title}</span>
                      </span>
                    ))}
                  </div>
                ) : (
                  <div className="workspace-card__empty"><span>No windows yet</span></div>
                )}
                <div className="workspace-card__meta">
                  <span className="workspace-card__name">
                    {workspace.id === activeWorkspace && <span className="workspace-dot workspace-dot--active"/>}
                    <span className="control-label">Workspace {workspace.id + 1}</span>
                  </span>
                  <span className="workspace-card__count control-label">
                    {workspace.windows.length === 0
                      ? "Empty"
                      : `${workspace.windows.length} ${workspace.windows.length === 1 ? "window" : "windows"}`}
                  </span>
                </div>
              </button>
            ))}
          </div>
          <div className="overview__windows">
            <span className="section-label">WINDOWS</span>
            <div className="overview__window-list">
              {activeWindows.length === 0 ? (
                <span className="command-empty">No open windows on this workspace</span>
              ) : (
                activeWindows.map((window) => (
                  <div
                    key={window.id}
                    className={`win-card${window.minimized ? " win-card--minimized" : ""}`}
                  >
                    <div className="win-card__head">
                      <button
                        className="win-card__activate"
                        aria-label={`${window.minimized ? "Restore" : "Activate"} ${window.title}`}
                        onClick={() => onActivate(window.id)}
                      >
                        <img className="win-card__icon" src={window.icon} alt=""/>
                        <span className="win-card__title control-label">{window.title}</span>
                        {window.minimized && <span className="win-card__state control-label">Minimized</span>}
                      </button>
                      <div className="win-card__moves">
                        {workspaces.filter((workspace) => workspace.id !== activeWorkspace).map((workspace) => (
                          <button
                            key={workspace.id}
                            className="win-card__move"
                            aria-label={`Move ${window.title} to Workspace ${workspace.id + 1}`}
                            onClick={() => onMoveWindow(window.id, workspace.id)}
                          >
                            <span className="control-label">Move W{workspace.id + 1}</span>
                          </button>
                        ))}
                      </div>
                      <button
                        className="win-card__close"
                        aria-label={`Close ${window.title}`}
                        onClick={() => onCloseWindow(window.id)}
                      >
                        <CloseGlyph/>
                      </button>
                    </div>
                    <button
                      className="win-card__preview"
                      aria-label={`${window.minimized ? "Restore" : "Activate"} ${window.title}`}
                      onClick={() => onActivate(window.id)}
                    >
                      <img className="win-card__preview-icon" src={window.icon} alt=""/>
                      <span className="control-label">
                        {window.minimized ? "Restore window" : "Activate window"}
                      </span>
                    </button>
                  </div>
                ))
              )}
            </div>
          </div>
          <div className="overview__footer">
            <span className="overview__shortcuts">
              <span className="key-hint"><span className="control-label">Esc</span></span>
              <span>Close</span>
              <span className="key-hint"><span className="control-label">Ctrl</span></span>
              <span className="key-hint"><span className="control-label">Alt</span></span>
              <span>Left / Right to switch</span>
            </span>
            <span className="overview__count">Workspace {activeWorkspace + 1} active</span>
          </div>
        </div>
      </div>
    </>
  );
}

/** System status surface backed only by live desktop-owned state. */
export function SystemCenter({
  time,
  date,
  volume,
  muted,
  activeWorkspace,
  openWindows,
  onVolume,
  onMuted,
}: {
  time: string;
  date: string;
  volume: number;
  muted: boolean;
  activeWorkspace: number;
  openWindows: number;
  onVolume: (value: number) => void;
  onMuted: () => void;
}) {
  return (
    <div className="system-center" data-lite-focus-scope={true} onPointerDown={stopPanelPointer}>
      <div className="system-center__header">
        <span className="system-center__title">System Center</span>
        <div className="system-center__clock">
          <span className="system-center__time">{time}</span>
          <span>{date}</span>
        </div>
      </div>
      <div className="sc-audio-summary">
        <span className="sc-audio-summary__icon"><img src="assets/volume.png"/></span>
        <div className="sc-audio-summary__copy">
          <span className="sc-audio-summary__title">System audio</span>
          <span>{muted ? "Output is muted" : `Output level ${volume}%`}</span>
        </div>
        <button className="system-slider__mute" aria-label={muted ? "Unmute" : "Mute"} onClick={onMuted}>
          <span className="control-label">{muted ? "Unmute" : "Mute"}</span>
        </button>
      </div>
      <div className="sc-slider sc-slider--audio">
        <img className="sc-slider__icon" src="assets/volume.png"/>
        <span className="sc-slider__label">Volume</span>
        <span className="sc-slider__value">{muted ? "Muted" : `${volume}%`}</span>
        <RangeInput
          className="sc-slider__range"
          min={0}
          max={100}
          step={1}
          value={volume}
          onInput={onVolume}
        />
      </div>
      <div className="sc-session-card">
        <span className="sc-session-card__mark"><span/></span>
        <div className="sc-session-card__copy">
          <span className="sc-session-card__title">Desktop session active</span>
          <span>
            Workspace {activeWorkspace + 1} · {openWindows} {openWindows === 1 ? "window" : "windows"} open
          </span>
        </div>
      </div>
    </div>
  );
}
