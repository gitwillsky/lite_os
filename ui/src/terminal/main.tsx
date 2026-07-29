import React, { useEffect, useRef, useState } from "react";
import { connect, input, paste } from "lite:terminal";

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
      const left = 20 + column * 8;
      column += run.columns;
      runs.push(
        <span
          key={`${index}:${left}`}
          className="terminal__run"
          style={{
            left,
            top: 18 + index * 16,
            width: run.columns * 8,
            color: hex(run.fg),
            background: hex(run.bg),
            fontWeight: run.bold ? "bold" : "normal",
          }}
        >{run.text}</span>
      );
    }
  });
  const cursor = screen.cursor;
  const cursorWidth = cursor.shape === "bar" ? 2 : 8;
  const cursorHeight = cursor.shape === "underline" ? 2 : 16;
  const cursorTop = 18 + cursor.row * 16 + (cursor.shape === "underline" ? 14 : 0);
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
          left: 20 + cursor.column * 8,
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
