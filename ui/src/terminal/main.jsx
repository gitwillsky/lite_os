import React, { useEffect, useState } from "react";
import { connect, input } from "lite:terminal";

const hex = (value) => "#" + value.toString(16).padStart(6, "0");

export default function Terminal() {
  const [screen, setScreen] = useState(() => connect(["/bin/sh"]));
  useEffect(() => globalThis.liteTerminalSubscribe(setScreen), []);
  // Runs carry only their own text; the start column is implicit in the
  // concatenation order, and every cell is exactly 8x16 CSS px.
  const runs = [];
  screen.rows.forEach((row, index) => {
    let column = 0;
    for (const run of row) {
      const left = column * 8;
      column += run.text.length;
      runs.push(
        <text
          key={`${index}:${left}`}
          className="terminal__run"
          style={{
            left,
            top: index * 16,
            color: hex(run.fg),
            background: hex(run.bg),
            fontWeight: run.bold ? "bold" : "normal",
          }}
        >{run.text}</text>
      );
    }
  });
  return (
    <view className="terminal" tabIndex={0} style={{ background: hex(screen.background) }} onKeyDown={(event) => input(event)}>
      {runs}
      <view
        className="terminal__cursor"
        style={{ left: screen.cursor.column * 8, top: screen.cursor.row * 16, background: hex(screen.foreground) }}
      />
    </view>
  );
}
