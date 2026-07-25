import React, { useEffect, useState } from "react";
import { clock } from "lite:desktop";

/** Formats epoch seconds as the XP tray clock (`h:mm AM/PM`, UTC+8). */
function formatClock(epochSeconds: number) {
  const local = epochSeconds + 8 * 3600;
  const minutes = Math.floor(local / 60) % 60;
  const hours = Math.floor(local / 3600) % 24;
  const suffix = hours >= 12 ? "PM" : "AM";
  return `${hours % 12 || 12}:${String(minutes).padStart(2, "0")} ${suffix}`;
}

interface TaskbarProps {
  windows: LiteSurface[];
  activeId: number;
  startOpen: boolean;
  onStart: () => void;
  onActivate: (id: number) => void;
}

export function Taskbar({ windows, activeId, startOpen, onStart, onActivate }: TaskbarProps) {
  const [now, setNow] = useState(() => clock());
  useEffect(() => {
    // The tray clock only shows minutes, so a 5s poll hugs the minute boundary
    // without needing a calendar dependency in the guest.
    let timer: ReturnType<typeof setTimeout>;
    const tick = () => {
      setNow(clock());
      timer = setTimeout(tick, 5000);
    };
    timer = setTimeout(tick, 5000);
    return () => clearTimeout(timer);
  }, []);

  return (
    <div className="taskbar">
      <div className={`start ${startOpen ? "start--pressed" : ""}`} onClick={onStart}>
        <div className="start__mark">
          <div className="start__mark-row"><div className="start__pane start__pane--red"/><div className="start__pane start__pane--green"/></div>
          <div className="start__mark-row"><div className="start__pane start__pane--blue"/><div className="start__pane start__pane--yellow"/></div>
        </div>
        <span className="start__label">Start</span>
      </div>
      <div className="taskbar__tasks">
        {windows.map((window) => (
          <div key={window.id} className={`task ${window.id === activeId ? "task--active" : ""}`} onClick={() => onActivate(window.id)}>
            <img className="task__icon" src={window.icon}/><span>{window.title}</span>
          </div>
        ))}
      </div>
      <div className="tray"><img className="tray__icon" src="assets/speaker.png"/><span className="tray__clock">{formatClock(now)}</span></div>
    </div>
  );
}
