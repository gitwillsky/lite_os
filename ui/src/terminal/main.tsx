import React, { useEffect, useRef, useState } from "react";
import { connect, input, paste } from "lite:terminal";

const CELL_WIDTH = 8;
const CELL_HEIGHT = 16;
const hex = (value: number) => "#" + value.toString(16).padStart(6, "0");

export default function Terminal() {
  const [screen, setScreen] = useState(() => connect(["/bin/sh"]));
  const clearedBanner = useRef(false);
  useEffect(() => globalThis.liteTerminalSubscribe((next) => {
    setScreen(next);
    if (!clearedBanner.current
      && next.rows.some((row) => row.some((run) => run.text.includes("BusyBox")))) {
      clearedBanner.current = true;
      paste("clear\n");
    }
  }), []);
  // Runs carry their occupied VT columns separately from JavaScript string
  // length; otherwise UTF-16 astral characters and two-column CJK both move
  // following runs and backgrounds to the wrong grid position.
  const runs: React.ReactElement[] = [];
  screen.rows.forEach((row, index) => {
    let column = 0;
    for (const run of row) {
      // PTY resize uses the complete client area in fixed cells, so row zero
      // and column zero must start at the exact client origin. Adding visual
      // padding here creates an unreported blank row and clips the far edge.
      const left = column * CELL_WIDTH;
      column += run.columns;
      runs.push(
        <span
          key={`${index}:${left}`}
          className="terminal__run"
          style={{
            left,
            top: index * CELL_HEIGHT,
            width: run.columns * CELL_WIDTH,
            color: hex(run.fg),
            background: hex(run.bg),
            fontWeight: run.bold ? "bold" : "normal",
          }}
        >{run.text}</span>
      );
    }
  });
  const cursor = screen.cursor;
  const cursorWidth = cursor.shape === "bar" ? 2 : CELL_WIDTH;
  const cursorHeight = cursor.shape === "underline" ? 2 : CELL_HEIGHT;
  const cursorTop = cursor.row * CELL_HEIGHT
    + (cursor.shape === "underline" ? CELL_HEIGHT - 2 : 0);
  const handleKey = (event: LiteKeyEvent) => {
    const control = (event.modifiers & 2) !== 0;
    const shift = (event.modifiers & 1) !== 0;
    const superKey = (event.modifiers & 8) !== 0;
    if (event.value !== 0 && ((control && shift) || superKey) && event.code === 47) {
      void navigator.clipboard.readText().then(paste);
      return;
    }
    input(event);
  };
  return (
    <div className="aurora-root terminal" tabIndex={0} style={{ background: hex(screen.background) }} onKeyDown={(event) => handleKey(event as unknown as LiteKeyEvent)}>
      {runs}
      <div
        className={`terminal__cursor${cursor.blinking ? " terminal__cursor--blinking" : ""}`}
        style={{
          left: cursor.column * CELL_WIDTH,
          top: cursorTop,
          width: cursorWidth,
          height: cursorHeight,
          opacity: cursor.visible ? 1 : 0,
          background: hex(screen.foreground),
        }}
      />
    </div>
  );
}
