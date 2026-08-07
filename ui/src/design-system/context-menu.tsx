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

const MENU_MARGIN = 8;
const MENU_OUTER_WIDTH = 204;
const MENU_PADDING_BORDER = 14;
const MENU_ITEM_HEIGHT = 34;
const MENU_SEPARATOR_HEIGHT = 11;
const KEY_ESC = 1;

export function ContextMenu({ x, y, items, onClose }: ContextMenuProps) {
  const menuHeight = MENU_PADDING_BORDER + items.reduce(
    (height, item) => height + (item.separator ? MENU_SEPARATOR_HEIGHT : MENU_ITEM_HEIGHT),
    0,
  );
  const maxContentHeight = Math.max(
    MENU_ITEM_HEIGHT,
    window.innerHeight - MENU_MARGIN * 2 - MENU_PADDING_BORDER,
  );
  const outerHeight = Math.min(menuHeight, maxContentHeight + MENU_PADDING_BORDER);
  const left = Math.max(MENU_MARGIN, Math.min(x, window.innerWidth - MENU_OUTER_WIDTH - MENU_MARGIN));
  const top = Math.max(MENU_MARGIN, Math.min(y, window.innerHeight - outerHeight - MENU_MARGIN));
  const choose = (index: number) => {
    const item = items[index];
    if (!item || item.separator || item.disabled || !item.onSelect) return;
    item.onSelect();
    onClose();
  };
  return (
    <div
      className="context-menu"
      data-lite-focus-scope={true}
      style={{ left, top, maxHeight: maxContentHeight }}
      onPointerDown={(rawEvent) => {
        // The menu is a modal pointer barrier. Without stopping pointer-down,
        // a menu action also mutates the row or terminal selection underneath it.
        (rawEvent as unknown as LitePointerEvent).stopPropagation();
      }}
      onContextMenu={(rawEvent) => {
        (rawEvent as unknown as LitePointerEvent).stopPropagation();
      }}
      onKeyDown={(rawEvent) => {
        const event = rawEvent as unknown as LiteKeyEvent;
        event.stopPropagation();
        if (event.code === KEY_ESC && event.value === 1) onClose();
      }}
    >
      {items.map((item, index) => (
        item.separator ? (
          <div key={item.id} className="menu-separator"/>
        ) : (
          <button
            key={item.id}
            className="menu-item"
            disabled={item.disabled}
            onClick={() => choose(index)}
          >
            <span className="control-label">{item.label}</span>
          </button>
        )
      ))}
    </div>
  );
}
