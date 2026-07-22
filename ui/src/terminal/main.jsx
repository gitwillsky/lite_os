import React, { useEffect, useState } from "react";
import { connect, input, resize } from "lite:terminal";

export default function Terminal() {
  const [screen, setScreen] = useState(() => connect(["/bin/sh"]));
  useEffect(() => globalThis.liteTerminalSubscribe(setScreen), []);
  return (
    <view className="terminal" tabIndex={0} onKeyDown={(event) => input(event)} onResize={(event) => resize(event.width, event.height)}>
      {screen.rows.map((row, index) => <text key={index} className="terminal__row">{row}</text>)}
      <view className="terminal__cursor" style={{ left: screen.cursor.column * 8, top: screen.cursor.row * 16 }}/>
    </view>
  );
}
