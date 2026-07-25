import React, { useCallback, useRef } from "react";

/** Renders one Luna window frame while leaving client pixels owned by compositor. */
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
    const dx = event.x - origin.startX;
    const dy = event.y - origin.startY;
    let { x, y, width, height } = origin;
    if (origin.edge.includes("e")) width = origin.width + dx;
    if (origin.edge.includes("s")) height = origin.height + dy;
    if (origin.edge.includes("w")) { x = origin.x + dx; width = origin.width - dx; }
    if (origin.edge.includes("n")) { y = origin.y + dy; height = origin.height - dy; }
    onResize(id, { x, y, width, height, anchorRight: origin.edge.includes("w"), anchorBottom: origin.edge.includes("n"), right: origin.x + origin.width, bottom: origin.y + origin.height });
  }, [id, onResize]);
  // On release, flush any resize commit the throttle deferred so the final
  // rect always lands at the true end position, then drop the grab.
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
          <view className="caption-button" onClick={() => onMinimize(id)}><image className="caption-button__glyph" src="assets/glyph-min.png"/></view>
          <view className="caption-button" onClick={() => onToggleMaximize(id)}><image className="caption-button__glyph" src={maximized ? "assets/glyph-restore.png" : "assets/glyph-max.png"}/></view>
          <view className="caption-button caption-button--close" onClick={() => onClose(id)}><image className="caption-button__glyph" src="assets/glyph-close.png"/></view>
        </view>
        <view className="window__titlebar-glow" />
      </view>
      <view className="window__body">{children}</view>
      {!maximized && ["s", "e", "w", "se", "sw"].map((edge) => (
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
