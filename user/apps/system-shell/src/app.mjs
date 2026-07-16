const {
  RIGHT,
  BOTTOM,
  STRETCH_WIDTH,
  STRETCH_HEIGHT,
  WINDOW,
  TITLE_BAR,
  CLOSE,
  MINIMIZE,
  MAXIMIZE,
  RESTORE,
  TEXT_GRID,
  label,
  rectangle,
  view,
} = LiteUIApp;

function DesktopIcon({ x, y, accent, name }) {
  return view(rectangle(x, y, 144, 86, 0x008080), [
    view(rectangle(20, 4, 38, 42, accent, 0xffffff, 1)),
    label(rectangle(0, 52, 144, 32, 0x008080), name, 0xffffff),
  ]);
}

function ClassicWindow({ title }) {
  return view(rectangle(80, 70, 80, 110, 0x404040, 0, 0, STRETCH_WIDTH | STRETCH_HEIGHT, WINDOW), [
    view(rectangle(0, 0, 8, 8, 0xc0c0c0, 0xffffff, 2, STRETCH_WIDTH | STRETCH_HEIGHT), [
      view(rectangle(4, 4, 4, 32, title, 0, 0, STRETCH_WIDTH, TITLE_BAR), [
        label(rectangle(10, 0, 320, 32, title), "LiteOS Workstation", 0xffffff, true),
        view(rectangle(86, 5, 32, 22, 0xc0c0c0, 0xffffff, 2, RIGHT, MINIMIZE)),
        view(rectangle(48, 5, 32, 22, 0xc0c0c0, 0xffffff, 2, RIGHT, MAXIMIZE)),
        view(rectangle(10, 5, 32, 22, 0xc0c0c0, 0xffffff, 2, RIGHT, CLOSE)),
      ]),
      view(rectangle(4, 40, 4, 36, 0xc0c0c0, 0x808080, 1, STRETCH_WIDTH), [
        label(rectangle(14, 2, 64, 32, 0xc0c0c0), "File"),
        label(rectangle(82, 2, 64, 32, 0xc0c0c0), "Edit"),
        label(rectangle(150, 2, 64, 32, 0xc0c0c0), "View"),
      ]),
      view(rectangle(
        6,
        80,
        6,
        8,
        0x101418,
        0x808080,
        2,
        STRETCH_WIDTH | STRETCH_HEIGHT,
        TEXT_GRID,
      )),
    ]),
  ]);
}

function Taskbar() {
  return view(rectangle(0, 0, 0, 42, 0xc0c0c0, 0xffffff, 1, BOTTOM | STRETCH_WIDTH), [
    view(rectangle(5, 5, 112, 32, 0xc0c0c0, 0xffffff, 2), [
      view(rectangle(10, 7, 20, 18, 0x000080)),
      label(rectangle(38, 0, 72, 30, 0xc0c0c0), "Start", 0x000000, true),
    ]),
    view(rectangle(126, 5, 310, 32, 0xd4d0c8, 0x808080, 2, 0, RESTORE), [
      view(rectangle(12, 8, 24, 16, 0x000080)),
      label(rectangle(46, 0, 252, 30, 0xd4d0c8), "LiteOS Workstation"),
    ]),
    view(rectangle(6, 5, 170, 32, 0xc0c0c0, 0x808080, 2, RIGHT), [
      label(rectangle(34, 0, 112, 30, 0xc0c0c0), "Ready"),
    ]),
  ]);
}

function Desktop() {
  const [focused] = createSignal(true);
  const title = createMemo(() => focused() ? 0x000080 : 0x808080);
  return [
    createComponent(DesktopIcon, { x: 28, y: 28, accent: 0xc0c0c0, name: "Computer" }),
    createComponent(DesktopIcon, { x: 28, y: 136, accent: 0x000080, name: "Network" }),
    createComponent(DesktopIcon, { x: 28, y: 244, accent: 0xffffff, name: "Trash" }),
    createComponent(ClassicWindow, { title: title() }),
    createComponent(Taskbar, {}),
  ];
}

LiteUIApp.publish(() => createComponent(Desktop, {}), {
  background: 0x008080,
  opaque: true,
});
