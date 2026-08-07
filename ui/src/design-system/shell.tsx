import React, { useMemo, useState } from "react";
import { CloseGlyph } from "./window.tsx";

const KEY_ENTER = 28;

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
      <button className="topbar__brand" onClick={() => toggle("command")}>
        <span className="lite-mark"><span/></span>
        <span className="control-label">LiteOS</span>
      </button>
      <button className="workspace-switcher" onClick={() => toggle("overview")}>
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
      <button className="topbar__status" onClick={() => toggle("system")}>
        <img className="status-glyph" src="assets/volume.png"/>
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
          onClick={item.onClick}
        >
          <img src={item.icon}/>
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
      <div className="command-center">
        <div className="cc-sidebar">
          <div className="cc-sidebar__brand">
            <span className="lite-mark"><span/></span>
            <div className="cc-sidebar__brand-text">
              <span>LiteOS</span>
              <span className="cc-sidebar__sub">Workspace {activeWorkspace + 1}</span>
            </div>
          </div>
          <button className="cc-sidebar__all">
            <img src="assets/all-apps.png"/>
            <span className="control-label">All apps</span>
          </button>
        </div>
        <div className="cc-main">
          <div className="command-search">
            <span className="search-glyph"><span/></span>
            <input
              autoFocus={true}
              value={query}
              placeholder="Search applications"
              onInput={(event) => setQuery((event as unknown as { value: string }).value)}
              onKeyDown={(raw) => {
                const event = raw as unknown as LiteKeyEvent;
                if (event.code === KEY_ENTER && event.value !== 0 && matches[0]) {
                  onLaunch(matches[0].id);
                }
              }}
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
                  <img src={app.icon}/>
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
            <span className="cc-footer__hint">
              <span className="key-hint"><span className="control-label">Enter</span></span>
              <span>Open first result</span>
            </span>
            <div className="cc-session-actions">
              <button className="cc-session-action" onClick={onRestart}>
                <img src="assets/restart.png"/>
                <span className="control-label">Restart</span>
              </button>
              <button className="cc-session-action cc-session-action--danger" onClick={onShutdown}>
                <img src="assets/power.png"/>
                <span className="control-label">Power off</span>
              </button>
            </div>
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
  onCloseWindow,
  onClose,
}: {
  workspaces: WorkspaceView[];
  activeWorkspace: number;
  onActivate: (id: number) => void;
  onSelect: (id: number) => void;
  onCloseWindow: (id: number) => void;
  onClose: () => void;
}) {
  const activeWindows = workspaces.find((workspace) => workspace.id === activeWorkspace)?.windows ?? [];
  return (
    <>
      <button className="shell-scrim overview__scrim" aria-label="Close workspace overview" onClick={onClose}/>
      <div className="overview">
        <div className="overview__panel">
          <div className="overview__header">
            <div className="overview__title-group">
              <span className="overview__mark"><img src="assets/all-apps.png"/></span>
              <span className="overview__title">Workspaces</span>
            </div>
            <span className="overview__summary">{workspaces.length} workspaces</span>
          </div>
          <div className="workspace-list">
            {workspaces.map((workspace) => (
              <button
                key={workspace.id}
                className={`workspace-card${workspace.id === activeWorkspace ? " workspace-card--active" : ""}`}
                onClick={() => onSelect(workspace.id)}
              >
                {workspace.windows.length > 0 ? (
                  <div className="workspace-card__preview">
                    {workspace.windows.slice(0, 2).map((window, index) => (
                      <div
                        key={window.id}
                        className={`mini-window mini-window--${index}${window.minimized ? " mini-window--minimized" : ""}`}
                        onClick={(event) => {
                          event.stopPropagation();
                          onActivate(window.id);
                        }}
                      >
                        <img src={window.icon}/>
                        <span>{window.title}</span>
                      </div>
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
                  <button
                    key={window.id}
                    className={`win-card${window.minimized ? " win-card--minimized" : ""}`}
                    onClick={() => onActivate(window.id)}
                  >
                    <div className="win-card__head">
                      <img className="win-card__icon" src={window.icon}/>
                      <span className="win-card__title control-label">{window.title}</span>
                      {window.minimized && <span className="win-card__state control-label">Minimized</span>}
                      <span
                        className="win-card__close"
                        aria-label="Close window"
                        onClick={(event) => {
                          event.stopPropagation();
                          onCloseWindow(window.id);
                        }}
                      >
                        <CloseGlyph/>
                      </span>
                    </div>
                    <div className="win-card__preview"/>
                  </button>
                ))
              )}
            </div>
          </div>
          <div className="overview__footer">
            <span className="overview__escape">
              <span className="key-hint"><span className="control-label">Esc</span></span>
              <span>Close</span>
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
    <div className="system-center">
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
        <input
          className="range-input sc-slider__range"
          type="range"
          min={0}
          max={100}
          value={volume}
          onInput={(event) => onVolume(Number((event as unknown as { value: string }).value))}
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
