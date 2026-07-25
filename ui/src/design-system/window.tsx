import React, { useCallback, useRef, useState } from "react";
import { projectResize } from "./window-geometry.ts";
import type { ResizeCandidate, ResizeEdge, ResizeOrigin } from "./window-geometry.ts";

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
interface WindowProps {
  id: number;
  title: string;
  icon: string;
  active: boolean;
  bounds: LiteFrame;
  children?: React.ReactNode;
  onActivate: (id: number) => void;
  onClose: (id: number) => void;
  onMoveStart: (id: number, serial: number) => boolean;
  onMove: (id: number, x: number, y: number) => void;
  onResize: (id: number, rect: ResizeCandidate) => void;
  onResizeEnd: (id: number) => void;
  onMinimize: (id: number) => void;
  onToggleMaximize: (id: number) => void;
  maximized: boolean;
}

type CaptionButton = "min" | "max" | "close";

export function Window({ id, title, icon, active, bounds, children, onActivate, onClose, onMoveStart, onMove, onResize, onResizeEnd, onMinimize, onToggleMaximize, maximized }: WindowProps) {
  const drag = useRef<{ x: number; y: number; native: boolean } | null>(null);
  const beginDrag = useCallback((event: LitePointerEvent) => {
    onActivate(id);
    drag.current = {
      x: event.x - bounds.x,
      y: event.y - bounds.y,
      native: onMoveStart(id, event.serial),
    };
  }, [bounds.x, bounds.y, id, onActivate, onMoveStart]);
  const continueDrag = useCallback((event: LitePointerEvent) => {
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
  const resize = useRef<ResizeOrigin & { edge: ResizeEdge } | null>(null);
  const beginResize = useCallback((edge: ResizeEdge) => (event: LitePointerEvent) => {
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
  const continueResize = useCallback((event: LitePointerEvent) => {
    const origin = resize.current;
    if (!origin) return;
    onResize(id, projectResize(origin.edge, origin, { x: event.x, y: event.y }));
  }, [id, onResize]);
  // On release, flush the newest preview and commit it once so the final rect
  // lands at the true end position, then drop the grab.
  const endResize = useCallback(() => { resize.current = null; onResizeEnd(id); }, [id, onResizeEnd]);

  // Caption-button feedback. The renderer has no CSS pseudo-classes, so hover
  // and press are driven by state toggled from real pointer events: hover-in/out
  // (onPointerEnter/Leave) lightens the button, and press (onPointerDown/Up)
  // inverts the bevel to sunken. Handlers are memoized so the host keeps their
  // listener ids stable across renders (the compositor tracks hover by that id).
  const [hovered, setHovered] = useState<CaptionButton | null>(null);
  const [pressed, setPressed] = useState<CaptionButton | null>(null);
  const enter = useCallback((which: CaptionButton) => () => setHovered(which), []);
  const leave = useCallback((which: CaptionButton) => () => setHovered((current) => (current === which ? null : current)), []);
  const press = useCallback((which: CaptionButton) => () => setPressed(which), []);
  const release = useCallback(() => setPressed(null), []);
  // Memoize per-button handler bundles once so their identities are stable.
  const buttons = useRef({
    min: { enter: enter("min"), leave: leave("min"), press: press("min") },
    max: { enter: enter("max"), leave: leave("max"), press: press("max") },
    close: { enter: enter("close"), leave: leave("close"), press: press("close") },
  }).current;
  const captionClass = (which: CaptionButton) =>
    `caption-button${hovered === which ? " caption-button--hover" : ""}${pressed === which ? " caption-button--pressed" : ""}`;

  return (
    <div
      className={`window ${active ? "window--active" : "window--inactive"}`}
      style={{ left: bounds.x, top: bounds.y, width: bounds.width, height: bounds.height }}
      data-lite-window={id}
      onPointerDown={() => onActivate(id)}
    >
      <div className="window__titlebar" onPointerDown={(e) => beginDrag(e as unknown as LitePointerEvent)} onPointerMove={(e) => continueDrag(e as unknown as LitePointerEvent)} onPointerUp={endDrag} onDoubleClick={() => onToggleMaximize(id)}>
        <img className="window__icon" src={icon} />
        <span className="window__title">{title}</span>
        <div className="window__controls" onPointerDown={() => {}}>
          <div
            className={captionClass("min")}
            onPointerEnter={buttons.min.enter}
            onPointerLeave={buttons.min.leave}
            onPointerDown={buttons.min.press}
            onPointerUp={release}
            onClick={() => { release(); onMinimize(id); }}
          >
            <div className="caption-glyph caption-glyph--minimize"/>
          </div>
          <div
            className={captionClass("max")}
            onPointerEnter={buttons.max.enter}
            onPointerLeave={buttons.max.leave}
            onPointerDown={buttons.max.press}
            onPointerUp={release}
            onClick={() => { release(); onToggleMaximize(id); }}
          >
            {maximized
              ? <div className="caption-glyph caption-glyph--restore"><div className="caption-glyph__back"/><div className="caption-glyph__front"/></div>
              : <div className="caption-glyph caption-glyph--maximize"/>}
          </div>
          <div
            className={captionClass("close")}
            onPointerEnter={buttons.close.enter}
            onPointerLeave={buttons.close.leave}
            onPointerDown={buttons.close.press}
            onPointerUp={release}
            onClick={() => { release(); onClose(id); }}
          >
            <div className="caption-glyph--close">
              {CLOSE_GLYPH_PIXELS.map(([left, top]) => <div key={`${left}:${top}`} style={{ left, top }}/>)}
            </div>
          </div>
        </div>
      </div>
      <div className="window__body">{children}</div>
      {!maximized && (["n", "s", "e", "w", "ne", "nw", "se", "sw"] as ResizeEdge[]).map((edge) => (
        <div
          key={edge}
          className={`window__resize window__resize--${edge}`}
          onPointerDown={(e) => beginResize(edge)(e as unknown as LitePointerEvent)}
          onPointerMove={(e) => continueResize(e as unknown as LitePointerEvent)}
          onPointerUp={endResize}
        />
      ))}
    </div>
  );
}
