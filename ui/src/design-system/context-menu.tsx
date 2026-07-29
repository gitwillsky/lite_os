import React from "react";

/** Shared semantic context menu positioned in viewport coordinates. */
interface ContextMenuItem {
  id: string;
  label: string;
  onSelect?: () => void;
  /** Disabled rows render muted and never dispatch `onSelect`. */
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

export function ContextMenu({ x, y, items, onClose }: ContextMenuProps) {
  return (
    <div className="context-menu" style={{ left: x, top: y }}>
      {items.map((item) => (
        item.separator ? (
          <div key={item.id} className="menu-separator"/>
        ) : (
          <button
            key={item.id}
            className="menu-item"
            disabled={item.disabled}
            onClick={() => { item.onSelect?.(); onClose(); }}
          >
            <span>{item.label}</span>
          </button>
        )
      ))}
    </div>
  );
}
