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
        <img className="status-glyph" src="assets/wifi.png"/>
        <img className="status-glyph" src="assets/network.png"/>
        <img className="status-glyph" src="assets/battery.png"/>
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

/** A static recent-activity entry shown in the Command Center. */
interface RecentItem {
  id: string;
  name: string;
  icon: string;
  when: string;
}

// Presentational recents: LiteOS has no recent-activity index service yet, so
// the Command Center mirrors the concept mockup with a fixed sample list. Wire
// a real provider through a lite:* module before treating these as live.
const RECENT_ITEMS: RecentItem[] = [
  { id: "r1", name: "Project Aurora", icon: "assets/files.png", when: "Today, 10:15 AM" },
  { id: "r2", name: "Design Notes.txt", icon: "assets/file.png", when: "Today, 9:42 AM" },
  { id: "r3", name: "aurora-wallpaper.png", icon: "assets/file.png", when: "Yesterday, 6:31 PM" },
  { id: "r4", name: "ambient-track.mp3", icon: "assets/monitor.png", when: "Yesterday, 5:08 PM" },
];

interface QuickAction {
  id: string;
  label: string;
  icon: string;
  danger?: boolean;
  onClick?: () => void;
}

/** Search-first global command and application launcher. */
export function CommandCenter({
  apps,
  activeWorkspace,
  onLaunch,
  onClose,
  onShutdown,
}: {
  apps: CommandApp[];
  activeWorkspace: number;
  onLaunch: (id: string) => void;
  onClose: () => void;
  onShutdown: () => void;
}) {
  const [query, setQuery] = useState("");
  const matches = useMemo(() => {
    const normalized = query.trim().toLocaleLowerCase();
    return normalized
      ? apps.filter((app) => app.name.toLocaleLowerCase().includes(normalized))
      : apps;
  }, [apps, query]);
  // Lock / Sleep / Restart have no backing power service; they dismiss the
  // panel as placeholders. Only Power off maps to the real shutdown path.
  const quickActions: QuickAction[] = [
    { id: "lock", label: "Lock", icon: "assets/lock.png", onClick: onClose },
    { id: "sleep", label: "Sleep", icon: "assets/sleep.png", onClick: onClose },
    { id: "restart", label: "Restart", icon: "assets/restart.png", onClick: onClose },
    { id: "power", label: "Power off", icon: "assets/power.png", danger: true, onClick: onShutdown },
  ];
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
            <span>All apps</span>
          </button>
        </div>
        <div className="cc-main">
          <div className="command-search">
            <span className="search-glyph"/>
            <input
              autoFocus={true}
              value={query}
              placeholder="Search apps, files, and actions"
              onInput={(event) => setQuery((event as unknown as { value: string }).value)}
            />
            <span className="key-hint">Ctrl</span>
            <span className="key-hint">Space</span>
            <button className="cc-mic" aria-label="Voice search">
              <img src="assets/microphone.png"/>
            </button>
          </div>
          <div className="command-section">
            <span className="section-label">SUGGESTED</span>
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
          <div className="command-section">
            <span className="section-label">RECENT</span>
            <div className="cc-recent">
              {RECENT_ITEMS.map((item) => (
                <button key={item.id} className="cc-recent-row">
                  <img src={item.icon}/>
                  <span className="cc-recent-row__name">{item.name}</span>
                  <span className="cc-recent-row__when">{item.when}</span>
                </button>
              ))}
            </div>
          </div>
          <div className="command-section">
            <span className="section-label">QUICK ACTIONS</span>
            <div className="cc-quick">
              {quickActions.map((action) => (
                <button key={action.id} className={`cc-quick-btn${action.danger ? " cc-quick-btn--danger" : ""}`} onClick={action.onClick}>
                  <img src={action.icon}/>
                  <span>{action.label}</span>
                </button>
              ))}
            </div>
          </div>
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
            {/* No newWorkspace backend: workspace count is fixed at 3, so the add
                button is a disabled placeholder matching the concept layout. */}
            <button className="icon-button" aria-label="Add workspace" disabled={true}>
              <span className="overview__plus"><span/><span/></span>
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
                  <div className="workspace-card__empty"><span>No windows yet</span></div>
                )}
                <div className="workspace-card__meta">
                  <span className="workspace-card__name">
                    {workspace.id === activeWorkspace && <span className="workspace-dot workspace-dot--active"/>}
                    <span>Workspace {workspace.id + 1}</span>
                  </span>
                  <span className="workspace-card__count">
                    {workspace.windows.length === 0 ? "Empty" : `${workspace.windows.length} windows`}
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
                  <button key={window.id} className="win-card" onClick={() => onActivate(window.id)}>
                    <div className="win-card__head">
                      <img className="win-card__icon" src={window.icon}/>
                      <span className="win-card__title">{window.title}</span>
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
            <span className="overview__escape"><span className="key-hint">Esc</span> Close</span>
            <span className="overview__count">Workspace {activeWorkspace + 1} active</span>
          </div>
        </div>
      </div>
    </>
  );
}

interface QuickToggle {
  id: string;
  label: string;
  icon: string;
  accent?: "cyan" | "violet";
}

const QUICK_TOGGLES: QuickToggle[] = [
  { id: "wifi", label: "Wi-Fi", icon: "assets/wifi.png", accent: "cyan" },
  { id: "bluetooth", label: "Bluetooth", icon: "assets/bluetooth.png", accent: "cyan" },
  { id: "night-light", label: "Night Light", icon: "assets/night-light.png" },
  { id: "do-not-disturb", label: "Do Not Disturb", icon: "assets/do-not-disturb.png" },
  { id: "airplane", label: "Airplane mode", icon: "assets/airplane.png" },
  { id: "focus", label: "Focus", icon: "assets/focus.png", accent: "violet" },
];

interface Notification {
  id: string;
  icon: string;
  title: string;
  body: string;
  when: string;
}

const SAMPLE_NOTIFICATIONS: Notification[] = [
  { id: "n1", icon: "assets/files.png", title: "Files", body: "Download complete", when: "10:22" },
  { id: "n2", icon: "assets/package.png", title: "Software Update", body: "Installation finished", when: "09:48" },
  { id: "n3", icon: "assets/settings.png", title: "System", body: "Backup completed", when: "09:15" },
];

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
  // Presentational quick-settings state. Only volume is wired to a real
  // service (lite:audio-system); the toggle grid, brightness, battery and
  // speakers are local UI state / constants matching the concept mockup.
  const [toggles, setToggles] = useState<Record<string, boolean>>({
    wifi: true,
    bluetooth: true,
    focus: true,
  });
  const [brightness, setBrightness] = useState(72);
  const [notifications, setNotifications] = useState<Notification[]>(SAMPLE_NOTIFICATIONS);
  const flip = (id: string) => setToggles((current) => ({ ...current, [id]: !current[id] }));
  const dismiss = (id: string) =>
    setNotifications((current) => current.filter((item) => item.id !== id));

  return (
    <div className="system-center">
      <div className="system-center__header">
        <span className="system-center__title">System Center</span>
        <div className="system-center__clock">
          <span className="system-center__time">{time}</span>
          <span>{date}</span>
        </div>
      </div>
      <div className="sc-toggle-grid">
        {QUICK_TOGGLES.map((toggle) => {
          const on = Boolean(toggles[toggle.id]);
          const accent = on ? ` sc-toggle--on sc-toggle--${toggle.accent ?? "cyan"}` : "";
          return (
            <button key={toggle.id} className={`sc-toggle${accent}`} onClick={() => flip(toggle.id)}>
              <img src={toggle.icon}/>
              <span>{toggle.label}</span>
            </button>
          );
        })}
      </div>
      <div className="sc-slider">
        <img className="sc-slider__icon" src="assets/brightness.png"/>
        <span className="sc-slider__label">Brightness</span>
        <span className="sc-slider__value">{brightness}%</span>
        <input
          className="range-input sc-slider__range"
          type="range"
          min={0}
          max={100}
          value={brightness}
          onInput={(event) => setBrightness(Number((event as unknown as { value: string }).value))}
        />
      </div>
      <div className="sc-slider">
        <img className="sc-slider__icon" src="assets/volume.png"/>
        <span className="sc-slider__label">Volume</span>
        <button className="system-slider__mute" aria-label={muted ? "Unmute" : "Mute"} onClick={onMuted}>
          {muted ? "Unmute" : "Mute"}
        </button>
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
      <div className="sc-row">
        <img className="sc-row__icon" src="assets/speakers.png"/>
        <span className="sc-row__label">Speakers</span>
        <span className="sc-row__chev">›</span>
      </div>
      <div className="sc-row">
        <img className="sc-row__icon" src="assets/battery-lg.png"/>
        <span className="sc-row__label">Battery 86%</span>
        <span className="sc-row__meta">4h 12m remaining</span>
      </div>
      <div className="sc-notif-head">
        <span className="section-label">NOTIFICATIONS</span>
        {notifications.length > 0 && (
          <button className="sc-notif-clear" onClick={() => setNotifications([])}>Clear all</button>
        )}
      </div>
      {notifications.length === 0 ? (
        <div className="system-empty">
          <span>No notifications</span>
          <span>System messages will appear here.</span>
        </div>
      ) : (
        <div className="sc-notif-list">
          {notifications.map((item) => (
            <div key={item.id} className="sc-notif">
              <img className="sc-notif__icon" src={item.icon}/>
              <div className="sc-notif__text">
                <span className="sc-notif__title">{item.title}</span>
                <span className="sc-notif__body">{item.body}</span>
              </div>
              <span className="sc-notif__when">{item.when}</span>
              <button className="sc-notif__close" aria-label="Dismiss" onClick={() => dismiss(item.id)}>
                <CloseGlyph/>
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
