import React, { useEffect, useRef, useState } from "react";
import { connect, input, paste, scroll, select as selectCells } from "lite:terminal";
import { ContextMenu } from "../design-system/context-menu.tsx";

const CELL_WIDTH = 8;
const CELL_HEIGHT = 16;
const BTN_LEFT = 272;
const KEY_PAGE_UP = 104;
const KEY_PAGE_DOWN = 109;
const hex = (value: number) => "#" + value.toString(16).padStart(6, "0");

type CellPosition = { column: number; row: number };

export default function Terminal() {
  const [screen, setScreen] = useState(() => connect(["/bin/sh"]));
  const [notice, setNotice] = useState<string | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const clearedBanner = useRef(false);
  const selectionAnchor = useRef<CellPosition | null>(null);
  const noticeTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => globalThis.liteTerminalSubscribe((next) => {
    setScreen(next);
    if (!clearedBanner.current
      && next.rows.some((row) => row.some((run) => run.text.includes("BusyBox")))) {
      clearedBanner.current = true;
      paste("clear\n");
    }
  }), []);
  useEffect(() => () => {
    if (noticeTimer.current !== null) clearTimeout(noticeTimer.current);
  }, []);
  const showNotice = (message: string) => {
    setNotice(message);
    if (noticeTimer.current !== null) clearTimeout(noticeTimer.current);
    noticeTimer.current = setTimeout(() => {
      noticeTimer.current = null;
      setNotice(null);
    }, 1600);
  };
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
  const selectionRows: React.ReactElement[] = [];
  if (screen.selection) {
    for (let row = screen.selection.start.row; row <= screen.selection.end.row; row += 1) {
      const first = row === screen.selection.start.row ? screen.selection.start.column : 0;
      const last = row === screen.selection.end.row ? screen.selection.end.column : screen.columns - 1;
      selectionRows.push(
        <div
          key={`selection:${row}`}
          className="terminal__selection"
          style={{
            left: first * CELL_WIDTH,
            top: row * CELL_HEIGHT,
            width: (last - first + 1) * CELL_WIDTH,
            height: CELL_HEIGHT,
          }}
        />
      );
    }
  }
  const cellAt = (event: LitePointerEvent): CellPosition => ({
    column: Math.max(0, Math.min(screen.columns - 1, Math.floor(event.x / CELL_WIDTH))),
    row: Math.max(0, Math.min(screen.rows.length - 1, Math.floor(event.y / CELL_HEIGHT))),
  });
  const updateSelection = (anchor: CellPosition, focus: CellPosition) => {
    selectCells(anchor.column, anchor.row, focus.column, focus.row);
  };
  const copySelection = () => {
    if (!screen.selection?.text) return;
    void navigator.clipboard.writeText(screen.selection.text)
      .then(() => showNotice("Selection copied"))
      .catch(() => showNotice("Copy failed"));
  };
  const pasteClipboard = () => {
    void navigator.clipboard.readText()
      .then((text) => {
        paste(text);
        showNotice("Pasted");
      })
      .catch(() => showNotice("Paste failed"));
  };
  const handleKey = (event: LiteKeyEvent) => {
    setMenu(null);
    const control = (event.modifiers & 2) !== 0;
    const shift = (event.modifiers & 1) !== 0;
    const superKey = (event.modifiers & 8) !== 0;
    const clipboardChord = (control && shift) || superKey;
    if (shift && (event.code === KEY_PAGE_UP || event.code === KEY_PAGE_DOWN)) {
      if (event.value === 1) {
        const page = Math.max(1, Math.floor(screen.rows.length * 0.75));
        scroll(event.code === KEY_PAGE_UP ? page : -page);
      }
      return;
    }
    if (clipboardChord && event.code === 46) {
      if (event.value === 1) copySelection();
      return;
    }
    if (clipboardChord && event.code === 47) {
      // Consume the whole shortcut chord so the PTY never sees an unmatched
      // key-up; paste only on the initial press so key repeat cannot duplicate
      // clipboard contents.
      if (event.value === 1) pasteClipboard();
      return;
    }
    input(event);
  };
  return (
    <div
      className="aurora-root terminal"
      tabIndex={0}
      style={{ background: hex(screen.background) }}
      onKeyDown={(event) => handleKey(event as unknown as LiteKeyEvent)}
      onWheel={(rawEvent) => {
        const event = rawEvent as unknown as LiteWheelEvent;
        if (event.deltaY === 0) return;
        const lines = Math.max(1, Math.round(Math.abs(event.deltaY) / CELL_HEIGHT));
        scroll(event.deltaY < 0 ? lines : -lines);
      }}
      onContextMenu={(rawEvent) => {
        const event = rawEvent as unknown as LitePointerEvent;
        event.stopPropagation();
        setMenu({ x: event.x, y: event.y });
      }}
      onPointerDown={(rawEvent) => {
        const event = rawEvent as unknown as LitePointerEvent;
        if (event.button !== BTN_LEFT || screen.rows.length === 0 || screen.columns === 0) return;
        const anchor = cellAt(event);
        setMenu(null);
        selectionAnchor.current = anchor;
        updateSelection(anchor, anchor);
        setNotice(null);
      }}
      onPointerMove={(rawEvent) => {
        const anchor = selectionAnchor.current;
        if (!anchor) return;
        updateSelection(anchor, cellAt(rawEvent as unknown as LitePointerEvent));
      }}
      onPointerUp={(rawEvent) => {
        const anchor = selectionAnchor.current;
        if (!anchor) return;
        updateSelection(anchor, cellAt(rawEvent as unknown as LitePointerEvent));
        selectionAnchor.current = null;
      }}
    >
      {runs}
      {selectionRows}
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
      {notice && <div className="terminal__notice">{notice}</div>}
      {screen.scrollOffset > 0 && (
        <div className="terminal__scrollback">
          Scrollback {screen.scrollOffset} / {screen.historyRows} lines · Shift+PageDown to return
        </div>
      )}
      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          items={[
            { id: "copy", label: "Copy", disabled: !screen.selection?.text, onSelect: copySelection },
            { id: "paste", label: "Paste", onSelect: pasteClipboard },
            { id: "separator", label: "", separator: true },
            {
              id: "select-all",
              label: "Select visible screen",
              onSelect: () => {
                if (screen.rows.length > 0 && screen.columns > 0) {
                  selectCells(0, 0, screen.columns - 1, screen.rows.length - 1);
                }
              },
            },
          ]}
          onClose={() => setMenu(null)}
        />
      )}
    </div>
  );
}
