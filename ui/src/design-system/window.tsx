import React, { useCallback, useRef } from "react";
import { frameStyle, projectResize } from "./window-geometry.ts";
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

// Window controls share the titlebar's layout, but not its drag gesture. Without
// this boundary, pressing a control bubbles into beginDrag and publishes a
// MoveComplete after the control action has already changed window policy.
const stopControlPropagation = (event: { stopPropagation(): void }) => event.stopPropagation();

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
  onMoveStart: (id: number, serial: number) => void;
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
  onResize,
  onResizeEnd,
  onMinimize,
  onToggleMaximize,
  maximized,
}: WindowProps) {
  const resize = useRef<ResizeOrigin & { edge: ResizeEdge } | null>(null);

  const beginDrag = useCallback((event: LitePointerEvent) => {
    onActivate(id);
    onMoveStart(id, event.serial);
  }, [id, onActivate, onMoveStart]);

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
      style={frameStyle(bounds)}
      data-lite-window={id}
      onPointerDown={() => onActivate(id)}
    >
      <div
        className="window__titlebar"
        onPointerDown={(event) => beginDrag(event as unknown as LitePointerEvent)}
        onDoubleClick={() => onToggleMaximize(id)}
      >
        <span className={`window__icon-frame window__icon-frame--${appId}`}>
          <img className="window__icon" src={icon} alt=""/>
        </span>
        <span className="window__title">{title}</span>
        <div
          className="window__controls"
          onPointerDown={stopControlPropagation}
          onDoubleClick={stopControlPropagation}
        >
          <button className="window-control" aria-label="Minimize" onClick={() => onMinimize(id)}>
            <span className="window-control__minimize"/>
          </button>
          <button className="window-control" aria-label={maximized ? "Restore" : "Maximize"} onClick={() => onToggleMaximize(id)}>
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
