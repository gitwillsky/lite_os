import React, { useEffect, useState } from "react";
import { clock } from "lite:desktop";
import { getState, setMuted, setVolume, subscribe } from "lite:audio-system";

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
  const [audioOpen, setAudioOpen] = useState(false);
  const [master, setMaster] = useState({ percent: 75, muted: false });
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
  useEffect(() => {
    const unsubscribe = subscribe((state) => {
      setMaster({ percent: state.percent, muted: state.muted });
    });
    getState();
    return unsubscribe;
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
      <div className="tray">
        {audioOpen && (
          <div className="tray-volume">
            <span className="tray-volume__title">Master volume</span>
            <div className="tray-volume__scale">
              {Array.from({ length: 11 }, (_, index) => index * 10).map((percent) => (
                <div
                  key={percent}
                  className={`tray-volume__step${percent <= master.percent ? " tray-volume__step--on" : ""}`}
                  onClick={() => setVolume(percent)}
                />
              ))}
            </div>
            <span className="tray-volume__value">{master.muted ? "Muted" : `${master.percent}%`}</span>
            <div className="tray-volume__mute" onClick={() => setMuted(!master.muted)}>
              {master.muted ? "Unmute" : "Mute"}
            </div>
          </div>
        )}
        <img
          className="tray__icon"
          src="assets/speaker.png"
          onClick={() => setAudioOpen((open) => !open)}
        />
        <span className="tray__clock">{formatClock(now)}</span>
      </div>
    </div>
  );
}
