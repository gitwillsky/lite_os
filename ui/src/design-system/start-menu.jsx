import React from "react";

export function StartMenu({ apps, onLaunch, onShutdown }) {
  return (
    <view className="start-menu" overlay={true}>
      <view className="start-menu__rail"><text>L</text><text>I</text><text>T</text><text>E</text><text>O</text><text>S</text></view>
      <view className="start-menu__items">
        {apps.map((app) => (
          <view key={app.id} className="classic-menu-item classic-menu-item--app" onClick={() => onLaunch(app.id)}>
            <image src={app.icon}/><text>{app.name}</text>
          </view>
        ))}
        <view className="menu-separator"/>
        <view className="classic-menu-item"><text>Programs</text><text className="classic-menu-item__arrow">&gt;</text></view>
        <view className="classic-menu-item"><text>Documents</text><text className="classic-menu-item__arrow">&gt;</text></view>
        <view className="classic-menu-item"><text>Settings</text><text className="classic-menu-item__arrow">&gt;</text></view>
        <view className="classic-menu-item"><text>Search</text></view>
        <view className="classic-menu-item"><text>Help and Support</text></view>
        <view className="classic-menu-item"><text>Run...</text></view>
        <view className="menu-separator"/>
        <view className="classic-menu-item"><text>Log Off LiteOS...</text></view>
        <view className="classic-menu-item" onClick={onShutdown}><text>Shut Down...</text></view>
      </view>
    </view>
  );
}
