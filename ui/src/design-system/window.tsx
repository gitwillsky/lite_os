import React, { useCallback, useRef } from "react";
import { projectResize } from "./window-geometry.ts";
import type { ResizeCandidate, ResizeEdge, ResizeOrigin } from "./window-geometry.ts";

/** CSS-drawn close glyph shared by every system-owned dismiss control. */
export function CloseGlyph() {
  return (
    <span className="window-control__close">
      <span className="window-control__close-a"/>
      <span className="window-control__close-b"/>
    </span>
  );
}

/** Props for the single system-owned Aurora window frame. */
interface WindowProps {
  id: number;
  appId: string;
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

/**
 * Renders the unique system window chrome around a compositor-owned app
 * surface. Applications own only their client content and never duplicate
 * title bars, window controls, resize grips, radii or focus treatment.
 */
export function Window({
  id,
  appId,
  title,
  icon,
  active,
  bounds,
  children,
  onActivate,
  onClose,
  onMoveStart,
  onMove,
  onResize,
  onResizeEnd,
  onMinimize,
  onToggleMaximize,
  maximized,
}: WindowProps) {
  const drag = useRef<{ x: number; y: number; native: boolean } | null>(null);
  const resize = useRef<ResizeOrigin & { edge: ResizeEdge } | null>(null);

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
  const endDrag = useCallback(() => {
    drag.current = null;
  }, []);

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
  }, [bounds, id, onActivate]);
  const continueResize = useCallback((event: LitePointerEvent) => {
    const origin = resize.current;
    if (origin) {
      onResize(id, projectResize(origin.edge, origin, { x: event.x, y: event.y }));
    }
  }, [id, onResize]);
  const endResize = useCallback(() => {
    resize.current = null;
    onResizeEnd(id);
  }, [id, onResizeEnd]);

  return (
    <div
      className={`window${active ? " window--active" : ""}`}
      style={{ left: bounds.x, top: bounds.y, width: bounds.width, height: bounds.height }}
      data-lite-window={id}
      onPointerDown={() => onActivate(id)}
    >
      <div
        className="window__titlebar"
        onPointerDown={(event) => beginDrag(event as unknown as LitePointerEvent)}
        onPointerMove={(event) => continueDrag(event as unknown as LitePointerEvent)}
        onPointerUp={endDrag}
        onDoubleClick={() => onToggleMaximize(id)}
      >
        <span className={`window__icon-frame window__icon-frame--${appId}`}>
          <img className="window__icon" src={icon}/>
        </span>
        <span className="window__title">{title}</span>
        <div className="window__controls">
          <button className="window-control" aria-label="Minimize" onClick={() => onMinimize(id)}>
            <span className="window-control__minimize"/>
          </button>
          <button className="window-control" aria-label="Maximize" onClick={() => onToggleMaximize(id)}>
            <span className={maximized ? "window-control__restore" : "window-control__maximize"}/>
          </button>
          <button className="window-control window-control--close" aria-label="Close" onClick={() => onClose(id)}>
            <CloseGlyph/>
          </button>
        </div>
      </div>
      <div className="window__body">{children}</div>
      {!maximized && (["n", "s", "e", "w", "ne", "nw", "se", "sw"] as ResizeEdge[]).map((edge) => (
        <div
          key={edge}
          className={`window__resize window__resize--${edge}`}
          onPointerDown={(event) => beginResize(edge)(event as unknown as LitePointerEvent)}
          onPointerMove={(event) => continueResize(event as unknown as LitePointerEvent)}
          onPointerUp={endResize}
        />
      ))}
    </div>
  );
}
