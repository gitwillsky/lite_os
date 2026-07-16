const {
  RIGHT,
  WINDOW,
  TITLE_BAR,
  CLOSE,
  MINIMIZE,
  button,
  label,
  rectangle,
  view,
} = LiteUIApp;

const keys = [
  ["7", "8", "9", "/"],
  ["4", "5", "6", "*"],
  ["1", "2", "3", "-"],
  ["0", ".", "=", "+"],
];

function Calculator() {
  const [display, setDisplay] = createSignal("0");
  let accumulator = 0;
  let operator = null;
  let replace = true;
  function result(value) {
    const text = Number.isFinite(value) ? String(value).slice(0, 24) : "Error";
    setDisplay(text);
    replace = true;
  }
  function press(key) {
    if ((key >= "0" && key <= "9") || key === ".") {
      const previous = replace || display() === "Error" ? "" : display();
      if (key === "." && previous.includes(".")) return;
      setDisplay((previous + key).slice(0, 24) || "0");
      replace = false;
      return;
    }
    if (key === "=") {
      const value = Number(display());
      if (operator === "+") result(accumulator + value);
      else if (operator === "-") result(accumulator - value);
      else if (operator === "*") result(accumulator * value);
      else if (operator === "/") result(value === 0 ? Number.NaN : accumulator / value);
      operator = null;
      return;
    }
    accumulator = Number(display());
    operator = key;
    replace = true;
  }
  const displayNode = label(
    rectangle(18, 50, 284, 34, 0xffffff, 0x808080, 2),
    "0",
    0x000000,
    true,
  );
  createEffect(() => {
    displayNode.text = display();
  });
  const buttons = [];
  for (let row = 0; row < keys.length; row += 1) {
    for (let column = 0; column < keys[row].length; column += 1) {
      const key = keys[row][column];
      buttons.push(button(
        rectangle(18 + column * 74, 92 + row * 54, 62, 42, 0xc0c0c0, 0xffffff, 2),
        key,
        () => press(key),
      ));
    }
  }
  return view(rectangle(420, 180, 340, 350, 0xc0c0c0, 0xffffff, 2, 0, WINDOW), [
    view(rectangle(4, 4, 332, 34, 0x000080, 0, 0, 0, TITLE_BAR), [
      label(rectangle(10, 0, 190, 32, 0x000080), "Calculator", 0xffffff, true),
      view(rectangle(48, 6, 30, 22, 0xc0c0c0, 0xffffff, 2, RIGHT, MINIMIZE)),
      view(rectangle(10, 6, 30, 22, 0xc0c0c0, 0xffffff, 2, RIGHT, CLOSE)),
    ]),
    displayNode,
    ...buttons,
  ]);
}

LiteUIApp.publish(() => createComponent(Calculator, {}), {
  background: 0,
  opaque: false,
});
