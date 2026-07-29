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
interface ContextMenuItem {
  id: string;
  label: string;
  onSelect?: () => void;
  /** Disabled rows render grayed and never dispatch `onSelect` (XP semantics). */
  disabled?: boolean;
  /** A separator renders the etched divider row; its label is ignored. */
  separator?: boolean;
}

interface ContextMenuProps {
  x: number;
  y: number;
  items: ContextMenuItem[];
  onClose: () => void;
}

interface HoverHandlers {
  onPointerEnter: () => void;
  onPointerLeave: () => void;
}

export function ContextMenu({ x, y, items, onClose }: ContextMenuProps) {
  const [hovered, setHovered] = useState<string | null>(null);
  const enter = useCallback((id: string) => setHovered(id), []);
  const leave = useCallback((id: string) => setHovered((current) => (current === id ? null : current)), []);
  // Cache hover handlers per id so their identities — and thus the host listener
  // ids the compositor tracks hover by — stay stable across renders.
  const cache = useRef(new Map<string, HoverHandlers>()).current;
  const handlers = useCallback((id: string) => {
    let bundle = cache.get(id);
    if (!bundle) {
      bundle = { onPointerEnter: () => enter(id), onPointerLeave: () => leave(id) };
      cache.set(id, bundle);
    }
    return bundle;
  }, [cache, enter, leave]);
  const itemClass = (id: string) =>
    `classic-menu-item${hovered === id ? " classic-menu-item--hover" : ""}`;

  return (
    <div className="context-menu" style={{ left: x, top: y }}>
      {items.map((item) => (
        item.separator ? (
          <div key={item.id} className="menu-separator"/>
        ) : (
          <div
            key={item.id}
            className={`${itemClass(item.id)}${item.disabled ? " classic-menu-item--disabled" : ""}`}
            {...(item.disabled ? {} : handlers(item.id))}
            onClick={() => { if (!item.disabled) { item.onSelect?.(); onClose(); } }}
          >
            <span>{item.label}</span>
          </div>
        )
      ))}
    </div>
  );
}
