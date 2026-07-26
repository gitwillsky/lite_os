import React from "react";

/**
 * A small read-only classic properties dialog, positioned at a click point like
 * {@link ContextMenu}. It reuses the shared `.context-menu` overlay chrome for a
 * single owner of popup styling, and lists label/value rows plus a dismiss row.
 *
 * @param props.x Viewport-local left in logical pixels.
 * @param props.y Viewport-local top in logical pixels.
 * @param props.title Heading (e.g. the item name).
 * @param props.rows `[label, value]` pairs shown one per line.
 * @param props.onClose Dismisses the popup (also fired by the OK row).
 */
interface PropertiesPopupProps {
  x: number;
  y: number;
  title: string;
  rows: [string, string][];
  onClose: () => void;
}

export function PropertiesPopup({ x, y, title, rows, onClose }: PropertiesPopupProps) {
  return (
    <div className="properties-popup" style={{ left: x, top: y }} onClick={onClose}>
      <div className="properties-popup__title"><span>{title}</span></div>
      {rows.map(([label, value]) => (
        <div key={label} className="properties-popup__row">
          <span className="properties-popup__label">{label}</span>
          <span className="properties-popup__value">{value}</span>
        </div>
      ))}
      <div className="properties-popup__ok"><span>OK</span></div>
    </div>
  );
}
