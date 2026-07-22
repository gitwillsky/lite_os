import React, { useEffect, useState } from "react";
import { clock } from "lite:desktop";

/** Formats epoch seconds as the XP tray clock (`h:mm AM/PM`, UTC). */
function formatClock(epochSeconds) {
  const minutes = Math.floor(epochSeconds / 60) % 60;
  const hours = Math.floor(epochSeconds / 3600) % 24;
  const suffix = hours >= 12 ? "PM" : "AM";
  return `${hours % 12 || 12}:${String(minutes).padStart(2, "0")} ${suffix}`;
}

export function Taskbar({ windows, activeId, startOpen, onStart, onActivate }) {
  const [now, setNow] = useState(() => clock());
  useEffect(() => {
    // The tray clock only shows minutes, so a 5s poll hugs the minute boundary
    // without needing a calendar dependency in the guest.
    let timer;
    const tick = () => {
      setNow(clock());
      timer = setTimeout(tick, 5000);
    };
    timer = setTimeout(tick, 5000);
    return () => clearTimeout(timer);
  }, []);

  return (
    <view className="taskbar">
      <view className={`start ${startOpen ? "start--pressed" : ""}`} onClick={onStart}>
        <text className="start__label">start</text>
      </view>
      <view className="taskbar__tasks">
        {windows.map((window) => (
          <view key={window.id} className={`task ${window.id === activeId ? "task--active" : ""}`} onClick={() => onActivate(window.id)}>
            <image className="task__icon" src={window.icon}/><text>{window.title}</text>
          </view>
        ))}
      </view>
      <view className="tray"><image className="tray__icon" src="assets/speaker.png"/><text className="tray__clock">{formatClock(now)}</text></view>
    </view>
  );
}
