import React from "react";

export function StartMenu({ apps, onLaunch, onShutdown }) {
  return (
    <view className="start-menu" overlay={true}>
      <view className="start-menu__user"><image src="assets/avatar.png"/><text>LiteOS</text></view>
      <view className="start-menu__columns">
        <view className="start-menu__primary">
          {apps.map((app) => <view key={app.id} className="menu-app" onClick={() => onLaunch(app.id)}><image src={app.icon}/><view><text className="menu-app__name">{app.name}</text><text className="menu-app__hint">{app.description}</text></view></view>)}
          <view className="start-menu__spacer"/>
          <view className="menu-separator"/>
          <view className="all-programs"><text className="all-programs__label">All Programs</text><image className="all-programs__arrow" src="assets/arrow-right.png"/></view>
        </view>
        <view className="start-menu__secondary">
          <text className="menu-link">My Documents</text><text className="menu-link">My Pictures</text><text className="menu-link">My Computer</text>
          <view className="menu-separator"/><text className="menu-link">Control Panel</text><text className="menu-link">Help and Support</text><text className="menu-link">Search</text><text className="menu-link">Run...</text>
        </view>
      </view>
      <view className="start-menu__footer">
        <view className="footer-action"><image src="assets/logoff.png"/><text>Log Off</text></view>
        <view className="footer-action" onClick={onShutdown}><image src="assets/power.png"/><text>Turn Off Computer</text></view>
      </view>
    </view>
  );
}
