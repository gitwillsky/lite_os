import React, { useCallback, useRef } from "react";

/** Renders one Luna window frame while leaving client pixels owned by compositor. */
export function Window({ id, title, icon, active, bounds, children, onActivate, onClose, onMove }) {
  const drag = useRef(null);
  const beginDrag = useCallback((event) => {
    onActivate(id);
    drag.current = { x: event.x - bounds.x, y: event.y - bounds.y };
  }, [bounds.x, bounds.y, id, onActivate]);
  const continueDrag = useCallback((event) => {
    if (drag.current) onMove(id, event.x - drag.current.x, event.y - drag.current.y);
  }, [id, onMove]);
  const endDrag = useCallback(() => { drag.current = null; }, []);

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
        <view className="window__controls">
          <view className="caption-button"><image className="caption-button__glyph" src="assets/glyph-min.png"/></view>
          <view className="caption-button"><image className="caption-button__glyph" src="assets/glyph-max.png"/></view>
          <view className="caption-button caption-button--close" onClick={() => onClose(id)}><image className="caption-button__glyph" src="assets/glyph-close.png"/></view>
        </view>
      </view>
      <view className="window__body">{children}</view>
    </view>
  );
}
