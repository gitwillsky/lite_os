import React, { useCallback, useRef, useState } from "react";
import type { AppMeta } from "lite:apps";

interface HoverHandlers {
  onPointerEnter: () => void;
  onPointerLeave: () => void;
}

interface StartMenuProps {
  apps: AppMeta[];
  onLaunch: (id: string) => void;
  onShutdown: () => void;
}

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

export function StartMenu({ apps, onLaunch, onShutdown }: StartMenuProps) {
  // The renderer has no CSS :hover; the classic blue highlight bar follows the
  // pointer via the real onPointerEnter/onPointerLeave events. Handlers are
  // cached per item id so their identities — and thus the host listener ids the
  // compositor tracks hover by — stay stable across renders.
  const [hovered, setHovered] = useState<string | null>(null);
  const enter = useCallback((id: string) => setHovered(id), []);
  const leave = useCallback((id: string) => setHovered((current) => (current === id ? null : current)), []);
  const cache = useRef(new Map<string, HoverHandlers>()).current;
  const handlers = useCallback((id: string) => {
    let bundle = cache.get(id);
    if (!bundle) {
      bundle = { onPointerEnter: () => enter(id), onPointerLeave: () => leave(id) };
      cache.set(id, bundle);
    }
    return bundle;
  }, [cache, enter, leave]);
  const itemClass = (id: string, extra = "") =>
    `classic-menu-item${extra}${hovered === id ? " classic-menu-item--hover" : ""}`;

  return (
    <div className="start-menu">
      <div className="start-menu__rail"><span>L</span><span>I</span><span>T</span><span>E</span><span>O</span><span>S</span></div>
      <div className="start-menu__items">
        {apps.map((app) => (
          <div
            key={app.id}
            className={itemClass(app.id, " classic-menu-item--app")}
            {...handlers(app.id)}
            onClick={() => onLaunch(app.id)}
          >
            <img src={app.icon}/><span>{app.name}</span>
          </div>
        ))}
        <div className="menu-separator"/>
        {STATIC_ITEMS.map((item) => (
          <div
            key={item.id}
            className={itemClass(item.id)}
            {...handlers(item.id)}
          >
            <span>{item.label}</span>
            {item.arrow && <span className="classic-menu-item__arrow">&gt;</span>}
          </div>
        ))}
        <div className="menu-separator"/>
        <div
          className={itemClass("logoff")}
          {...handlers("logoff")}
        >
          <span>Log Off LiteOS...</span>
        </div>
        <div
          className={itemClass("shutdown")}
          {...handlers("shutdown")}
          onClick={onShutdown}
        >
          <span>Shut Down...</span>
        </div>
      </div>
    </div>
  );
}
