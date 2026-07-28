import React, { useEffect, useState } from "react";
import { connect, input, paste } from "lite:terminal";

const hex = (value: number) => "#" + value.toString(16).padStart(6, "0");

export default function Terminal() {
  const [screen, setScreen] = useState(() => connect(["/bin/sh"]));
  const [cursorPhase, setCursorPhase] = useState(true);
  useEffect(() => globalThis.liteTerminalSubscribe(setScreen), []);
  useEffect(() => {
    setCursorPhase(true);
    if (!screen.cursor.blinking) return undefined;
    let timer: ReturnType<typeof setTimeout>;
    const tick = () => {
      setCursorPhase((visible) => !visible);
      timer = setTimeout(tick, 530);
    };
    timer = setTimeout(tick, 530);
    return () => clearTimeout(timer);
  }, [screen.cursor.blinking]);
  // Runs carry only their own text; the start column is implicit in the
  // concatenation order, and every cell is exactly 8x16 CSS px.
  const runs: React.ReactElement[] = [];
  screen.rows.forEach((row, index) => {
    let column = 0;
    for (const run of row) {
      const left = column * 8;
      column += run.text.length;
      runs.push(
        <span
          key={`${index}:${left}`}
          className="terminal__run"
          style={{
            left,
            top: index * 16,
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
  const cursorTop = cursor.row * 16 + (cursor.shape === "underline" ? 14 : 0);
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
    <div className="terminal" tabIndex={0} style={{ background: hex(screen.background) }} onKeyDown={(event) => handleKey(event as unknown as LiteKeyEvent)}>
      {runs}
      <div
        className="terminal__cursor"
        style={{
          left: cursor.column * 8,
          top: cursorTop,
          width: cursorWidth,
          height: cursorHeight,
          opacity: cursor.visible && cursorPhase ? 1 : 0,
          background: hex(screen.foreground),
        }}
      />
    </div>
  );
}
