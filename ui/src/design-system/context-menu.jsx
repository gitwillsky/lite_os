import React, { useCallback, useRef, useState } from "react";

/**
 * Classic right-click menu. Positioned at the click point via an inline
 * overlay (the same overlay + `style={{left,top}}` pattern the resize preview
 * uses), and reuses the Start menu's pointer-driven hover highlight since the
 * renderer has no CSS :hover. Every row dismisses the menu on click; rows with
 * an `onSelect` run it first.
 *
 * @param {{x:number, y:number, items:Array<{id:string,label:string,onSelect?:Function}>, onClose:Function}} props
 */
export function ContextMenu({ x, y, items, onClose }) {
  const [hovered, setHovered] = useState(null);
  const enter = useCallback((id) => setHovered(id), []);
  const leave = useCallback((id) => setHovered((current) => (current === id ? null : current)), []);
  // Cache hover handlers per id so their identities — and thus the host listener
  // ids the compositor tracks hover by — stay stable across renders.
  const cache = useRef(new Map()).current;
  const handlers = useCallback((id) => {
    let bundle = cache.get(id);
    if (!bundle) {
      bundle = { onPointerEnter: () => enter(id), onPointerLeave: () => leave(id) };
      cache.set(id, bundle);
    }
    return bundle;
  }, [cache, enter, leave]);
  const itemClass = (id) =>
    `classic-menu-item${hovered === id ? " classic-menu-item--hover" : ""}`;

  return (
    <view className="context-menu" overlay={true} style={{ left: x, top: y }}>
      {items.map((item) => (
        <view
          key={item.id}
          className={itemClass(item.id)}
          {...handlers(item.id)}
          onClick={() => { item.onSelect?.(); onClose(); }}
        >
          <text>{item.label}</text>
        </view>
      ))}
    </view>
  );
}
