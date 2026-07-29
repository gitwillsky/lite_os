import React, { useMemo, useState } from "react";
import { CloseGlyph } from "./window.tsx";

export type ShellPanel = "command" | "overview" | "system" | null;

interface TopBarProps {
  panel: ShellPanel;
  time: string;
  activeWorkspace: number;
  workspaceCount: number;
  onPanel: (panel: ShellPanel) => void;
}

/** System top bar: global entry, workspace state and system status. */
export function TopBar({
  panel,
  time,
  activeWorkspace,
  workspaceCount,
  onPanel,
}: TopBarProps) {
  const toggle = (next: Exclude<ShellPanel, null>) => onPanel(panel === next ? null : next);
  return (
    <div className="topbar">
      <button className="topbar__brand" onClick={() => toggle("command")}>
        <span className="lite-mark"><span/></span>
        <span>LiteOS</span>
      </button>
      <button className="workspace-switcher" onClick={() => toggle("overview")}>
        <span>Workspace {activeWorkspace + 1}</span>
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
        <span className="status-audio"><span/><span/><span/></span>
        <span>{time}</span>
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
        </button>
      ))}
    </div>
  );
}

interface CommandApp {
  id: string;
  name: string;
  icon: string;
}

/** Search-first global command and application launcher. */
export function CommandCenter({
  apps,
  onLaunch,
  onClose,
  onSystem,
  onShutdown,
}: {
  apps: CommandApp[];
  onLaunch: (id: string) => void;
  onClose: () => void;
  onSystem: () => void;
  onShutdown: () => void;
}) {
  const [query, setQuery] = useState("");
  const matches = useMemo(() => {
    const normalized = query.trim().toLocaleLowerCase();
    return normalized
      ? apps.filter((app) => app.name.toLocaleLowerCase().includes(normalized))
      : apps;
  }, [apps, query]);
  return (
    <>
      <button className="shell-scrim" aria-label="Close command center" onClick={onClose}/>
      <div className="command-center">
        <div className="command-search">
          <span className="search-glyph"/>
          <input
            autoFocus={true}
            value={query}
            placeholder="Search applications"
            onInput={(event) => setQuery((event as unknown as { value: string }).value)}
          />
        </div>
        <div className="command-section">
          <span className="section-label">Applications</span>
          <div className="command-grid">
            {matches.map((app) => (
              <button key={app.id} className="command-app" onClick={() => onLaunch(app.id)}>
                <img src={app.icon}/>
                <span>{app.name}</span>
              </button>
            ))}
            {matches.length === 0 && <span className="command-empty">No matching applications</span>}
          </div>
        </div>
        <div className="command-actions">
          <button onClick={onSystem}><span>⚙</span><span>System Center</span></button>
          <button onClick={onShutdown}><span>⏻</span><span>Shut Down</span></button>
        </div>
      </div>
    </>
  );
}

interface WorkspaceView {
  id: number;
  windows: LiteSurface[];
}

/** Spatial overview of running windows and available workspaces. */
export function WorkspaceOverview({
  workspaces,
  activeWorkspace,
  onActivate,
  onSelect,
  onClose,
}: {
  workspaces: WorkspaceView[];
  activeWorkspace: number;
  onActivate: (id: number) => void;
  onSelect: (id: number) => void;
  onClose: () => void;
}) {
  return (
    <div className="overview">
      <div className="overview__header">
        <div>
          <span className="overview__eyebrow">WORKSPACES</span>
          <span className="overview__title">Choose where to focus</span>
        </div>
        <button className="icon-button" aria-label="Close workspace overview" onClick={onClose}>
          <CloseGlyph/>
        </button>
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
                    className={`mini-window mini-window--${index}`}
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
              <div className="workspace-card__empty"><span>{workspace.id + 1}</span></div>
            )}
            <div className="workspace-card__meta">
              <span className="workspace-card__name">
                {workspace.id === activeWorkspace && <span className="workspace-dot workspace-dot--active"/>}
                Workspace {workspace.id + 1}
              </span>
              <span className="workspace-card__count">
                {workspace.windows.length === 0 ? "Empty" : `${workspace.windows.length} windows`}
              </span>
            </div>
          </button>
        ))}
      </div>
      <div className="overview__footer">
        <span className="overview__escape"><span className="key-hint">Esc</span> Close</span>
        <span className="overview__count">Workspace {activeWorkspace + 1} active</span>
      </div>
    </div>
  );
}

/** Unified quick settings and notifications surface. */
export function SystemCenter({
  time,
  date,
  volume,
  muted,
  onVolume,
  onMuted,
  onClose,
}: {
  time: string;
  date: string;
  volume: number;
  muted: boolean;
  onVolume: (value: number) => void;
  onMuted: () => void;
  onClose: () => void;
}) {
  return (
    <div className="system-center">
      <div className="system-center__header">
        <div><span className="system-center__time">{time}</span><span>{date}</span></div>
        <button className="icon-button" aria-label="Close system center" onClick={onClose}>
          <CloseGlyph/>
        </button>
      </div>
      <span className="system-section-label">SOUND</span>
      <div className="system-slider">
        <button className="system-slider__mute" aria-label={muted ? "Unmute" : "Mute"} onClick={onMuted}>
          {muted ? "Unmute" : "Mute"}
        </button>
        <input
          className="range-input"
          type="range"
          min={0}
          max={100}
          value={volume}
          onInput={(event) => onVolume(Number((event as unknown as { value: string }).value))}
        />
        <span>{muted ? "Muted" : `${volume}%`}</span>
      </div>
      <span className="system-section-label">NOTIFICATIONS</span>
      <div className="system-empty">
        <span>No notifications</span>
        <span>System messages will appear here.</span>
      </div>
    </div>
  );
}
