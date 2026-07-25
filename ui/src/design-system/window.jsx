import React, { useCallback, useRef } from "react";
import { projectResize } from "./window-geometry.js";

const CLOSE_GLYPH_PIXELS = [
  [0, 0], [1, 0], [6, 0], [7, 0],
  [0, 1], [1, 1], [2, 1], [5, 1], [6, 1], [7, 1],
  [1, 2], [2, 2], [3, 2], [4, 2], [5, 2], [6, 2],
  [2, 3], [3, 3], [4, 3], [5, 3],
  [2, 4], [3, 4], [4, 4], [5, 4],
  [1, 5], [2, 5], [3, 5], [4, 5], [5, 5], [6, 5],
  [0, 6], [1, 6], [2, 6], [5, 6], [6, 6], [7, 6],
  [0, 7], [1, 7], [6, 7], [7, 7],
];

/** Renders one classic window frame while leaving client pixels owned by compositor. */
export function Window({ id, title, icon, active, bounds, children, onActivate, onClose, onMoveStart, onMove, onResize, onResizeEnd, onMinimize, onToggleMaximize, maximized }) {
  const drag = useRef(null);
  const beginDrag = useCallback((event) => {
    onActivate(id);
    drag.current = {
      x: event.x - bounds.x,
      y: event.y - bounds.y,
      native: onMoveStart(id, event.serial),
    };
  }, [bounds.x, bounds.y, id, onActivate, onMoveStart]);
  const continueDrag = useCallback((event) => {
    if (drag.current && !drag.current.native) {
      onMove(id, event.x - drag.current.x, event.y - drag.current.y);
    }
  }, [id, onMove]);
  const endDrag = useCallback(() => { drag.current = null; }, []);

  // Edge/corner resize. Each grip captures which edges it drives on press, then
  // maps the pointer delta to a new rect: dragging a right/bottom edge grows the
  // size, dragging a left/top edge moves the origin AND shrinks the size so the
  // opposite edge stays put. `onResize` (the size-aware sibling of `onMove`)
  // clamps to the min size and work area.
  const resize = useRef(null);
  const beginResize = useCallback((edge) => (event) => {
    onActivate(id);
    resize.current = {
      edge,
      startX: event.x,
      startY: event.y,
      x: bounds.x,
      y: bounds.y,
      width: bounds.width,
      height: bounds.height,
    };
  }, [bounds.x, bounds.y, bounds.width, bounds.height, id, onActivate]);
  const continueResize = useCallback((event) => {
    const origin = resize.current;
    if (!origin) return;
    onResize(id, projectResize(origin.edge, origin, { x: event.x, y: event.y }));
  }, [id, onResize]);
  // On release, flush the newest preview and commit it once so the final rect
  // lands at the true end position, then drop the grab.
  const endResize = useCallback(() => { resize.current = null; onResizeEnd(id); }, [id, onResizeEnd]);

  return (
    <view
      className={`window ${active ? "window--active" : "window--inactive"}`}
      style={{ left: bounds.x, top: bounds.y, width: bounds.width, height: bounds.height }}
      windowGroup={id}
      onPointerDown={() => onActivate(id)}
    >
      <view className="window__titlebar" onPointerDown={beginDrag} onPointerMove={continueDrag} onPointerUp={endDrag}>
        <image className="window__icon" src={icon} />
        <text className="window__title">{title}</text>
        <view className="window__controls" onPointerDown={() => {}}>
          <view className="caption-button" onClick={() => onMinimize(id)}><view className="caption-glyph caption-glyph--minimize"/></view>
          <view className="caption-button" onClick={() => onToggleMaximize(id)}>
            {maximized
              ? <view className="caption-glyph caption-glyph--restore"><view className="caption-glyph__back"/><view className="caption-glyph__front"/></view>
              : <view className="caption-glyph caption-glyph--maximize"/>}
          </view>
          <view className="caption-button" onClick={() => onClose(id)}>
            <view className="caption-glyph--close">
              {CLOSE_GLYPH_PIXELS.map(([left, top]) => <view key={`${left}:${top}`} style={{ left, top }}/>)}
            </view>
          </view>
        </view>
      </view>
      <view className="window__body">{children}</view>
      {!maximized && ["n", "s", "e", "w", "ne", "nw", "se", "sw"].map((edge) => (
        <view
          key={edge}
          className={`window__resize window__resize--${edge}`}
          onPointerDown={beginResize(edge)}
          onPointerMove={continueResize}
          onPointerUp={endResize}
        />
      ))}
    </view>
  );
}
