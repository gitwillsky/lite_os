import React, { useCallback, useRef, useState } from "react";

// Static (non-app) menu rows. Stable ids let the compositor track hover by a
// stable listener identity across renders, matching the app rows keyed on app.id.
const STATIC_ITEMS = [
  { id: "programs", label: "Programs", arrow: true },
  { id: "documents", label: "Documents", arrow: true },
  { id: "settings", label: "Settings", arrow: true },
  { id: "search", label: "Search" },
  { id: "help", label: "Help and Support" },
  { id: "run", label: "Run..." },
];

export function StartMenu({ apps, onLaunch, onShutdown }) {
  // The renderer has no CSS :hover; the classic blue highlight bar follows the
  // pointer via the real onPointerEnter/onPointerLeave events. Handlers are
  // cached per item id so their identities — and thus the host listener ids the
  // compositor tracks hover by — stay stable across renders.
  const [hovered, setHovered] = useState(null);
  const enter = useCallback((id) => setHovered(id), []);
  const leave = useCallback((id) => setHovered((current) => (current === id ? null : current)), []);
  const cache = useRef(new Map()).current;
  const handlers = useCallback((id) => {
    let bundle = cache.get(id);
    if (!bundle) {
      bundle = { onPointerEnter: () => enter(id), onPointerLeave: () => leave(id) };
      cache.set(id, bundle);
    }
    return bundle;
  }, [cache, enter, leave]);
  const itemClass = (id, extra = "") =>
    `classic-menu-item${extra}${hovered === id ? " classic-menu-item--hover" : ""}`;

  return (
    <view className="start-menu" overlay={true}>
      <view className="start-menu__rail"><text>L</text><text>I</text><text>T</text><text>E</text><text>O</text><text>S</text></view>
      <view className="start-menu__items">
        {apps.map((app) => (
          <view
            key={app.id}
            className={itemClass(app.id, " classic-menu-item--app")}
            {...handlers(app.id)}
            onClick={() => onLaunch(app.id)}
          >
            <image src={app.icon}/><text>{app.name}</text>
          </view>
        ))}
        <view className="menu-separator"/>
        {STATIC_ITEMS.map((item) => (
          <view
            key={item.id}
            className={itemClass(item.id)}
            {...handlers(item.id)}
          >
            <text>{item.label}</text>
            {item.arrow && <text className="classic-menu-item__arrow">&gt;</text>}
          </view>
        ))}
        <view className="menu-separator"/>
        <view
          className={itemClass("logoff")}
          {...handlers("logoff")}
        >
          <text>Log Off LiteOS...</text>
        </view>
        <view
          className={itemClass("shutdown")}
          {...handlers("shutdown")}
          onClick={onShutdown}
        >
          <text>Shut Down...</text>
        </view>
      </view>
    </view>
  );
}
