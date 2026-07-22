import React from "react";

export function Taskbar({ windows, activeId, startOpen, onStart, onActivate }) {
  return (
    <view className="taskbar">
      <view className={`start ${startOpen ? "start--pressed" : ""}`} onClick={onStart}>
        <view className="start__flag"><view className="flag flag--red"/><view className="flag flag--green"/><view className="flag flag--blue"/><view className="flag flag--yellow"/></view>
        <text className="start__label">start</text>
      </view>
      <view className="taskbar__tasks">
        {windows.map((window) => (
          <view key={window.id} className={`task ${window.id === activeId ? "task--active" : ""}`} onClick={() => onActivate(window.id)}>
            <image className="task__icon" src={window.icon}/><text>{window.title}</text>
          </view>
        ))}
      </view>
      <view className="tray"><text className="tray__speaker">◖</text><text>2:59 PM</text></view>
    </view>
  );
}
