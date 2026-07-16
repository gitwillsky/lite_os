const LiteUIApp = (() => {
const ROOT = {
  id: 1,
  type: "root",
  parent: null,
  children: [],
  style: {
    x: 0,
    y: 0,
    width: 1920,
    height: 1080,
    background: 0,
    border: 0,
    borderWidth: 0,
    visible: false,
    anchors: 0,
    role: 0,
  },
};

function detach(node) {
  if (node.parent === null) return;
  const index = node.parent.children.indexOf(node);
  if (index >= 0) node.parent.children.splice(index, 1);
  node.parent = null;
}

const liteuiRenderer = createRenderer({
  createElement(type) {
    return { id: 0, type, parent: null, children: [], style: null };
  },
  createTextNode(value) {
    return { id: 0, type: "text", value: String(value), parent: null, children: [] };
  },
  isTextNode(node) {
    return node.type === "text";
  },
  replaceText(node, value) {
    node.value = String(value);
  },
  insertNode(parent, node, anchor) {
    detach(node);
    const index = anchor === undefined || anchor === null
      ? parent.children.length
      : parent.children.indexOf(anchor);
    parent.children.splice(index < 0 ? parent.children.length : index, 0, node);
    node.parent = parent;
  },
  removeNode(_parent, node) {
    detach(node);
  },
  setProperty(node, name, value) {
    if (name === "style") node.style = value;
    else if (name === "text") node.text = String(value);
    else if (name === "color") node.color = value;
    else if (name === "bold") node.bold = Boolean(value);
    else if (name === "onClick") node.onClick = value;
    else throw new Error(`unsupported LiteUI property: ${name}`);
  },
  getParentNode(node) {
    return node.parent;
  },
  getFirstChild(node) {
    return node.children[0];
  },
  getNextSibling(node) {
    if (node.parent === null) return undefined;
    const index = node.parent.children.indexOf(node);
    return node.parent.children[index + 1];
  },
});

function view(style, children = []) {
  const node = liteuiRenderer.createElement("view");
  liteuiRenderer.setProp(node, "style", style);
  for (const child of children) liteuiRenderer.insertNode(node, child);
  return node;
}

function label(style, text, color = 0x000000, bold = false) {
  const node = view(style);
  liteuiRenderer.setProp(node, "text", text);
  liteuiRenderer.setProp(node, "color", color);
  liteuiRenderer.setProp(node, "bold", bold);
  return node;
}

function button(style, text, onClick) {
  style.role = 7;
  const node = label(style, text, 0x000000, true);
  liteuiRenderer.setProp(node, "onClick", onClick);
  return node;
}

function rectangle(
  x,
  y,
  width,
  height,
  background,
  border = 0,
  borderWidth = 0,
  anchors = 0,
  role = 0,
) {
  return { x, y, width, height, background, border, borderWidth, visible: true, anchors, role };
}

function ascii(text) {
  const result = [];
  for (let index = 0; index < text.length; index += 1) {
    const byte = text.charCodeAt(index);
    if (byte === 0 || byte > 0x7f) throw new Error("phase-one LiteUI text must be ASCII");
    result.push(byte);
  }
  if (result.length === 0 || result.length > 24) {
    throw new Error("LiteUI text exceeds the inline run budget");
  }
  return result;
}

function publish(component, options) {
  ROOT.style.background = options.background;
  ROOT.style.visible = options.opaque;
  liteuiRenderer.render(component, ROOT);
  const ordered = [];
  function appendDepthFirst(node) {
    if (node.type === "text") return;
    if (ordered.length >= 255) throw new Error("LiteUI node budget exceeded");
    node.id = ordered.length + 1;
    ordered.push(node);
    for (const child of node.children) appendDepthFirst(child);
  }
  appendDepthFirst(ROOT);
  const operationCount = ordered.length
    + ordered.filter((node) => node.text !== undefined).length;
  if (operationCount > 256) throw new Error("LiteUI operation budget exceeded");
  const bytes = new Uint8Array(operationCount * 40);
  const data = new DataView(bytes.buffer);
  function commit(creating) {
    bytes.fill(0);
    let operation = 0;
    for (const node of ordered) {
      const style = node.style;
      if (style === null) throw new Error("LiteUI view is missing a typed style");
      let offset = operation * 40;
      data.setUint8(offset, node === ROOT || !creating ? 2 : 1);
      data.setUint8(offset + 1, style.visible ? 1 : 0);
      data.setUint16(offset + 2, node.id, true);
      data.setUint16(offset + 4, 1, true);
      if (creating && node.parent !== null) {
        data.setUint16(offset + 6, node.parent.id, true);
        data.setUint16(offset + 8, 1, true);
      }
      data.setInt32(offset + 12, style.x, true);
      data.setInt32(offset + 16, style.y, true);
      data.setInt32(offset + 20, style.width, true);
      data.setInt32(offset + 24, style.height, true);
      data.setUint32(offset + 28, style.background, true);
      data.setUint32(offset + 32, style.border, true);
      data.setUint8(offset + 36, style.borderWidth);
      data.setUint8(offset + 37, style.anchors);
      data.setUint8(offset + 38, style.role);
      operation += 1;
      if (node.text !== undefined) {
        const encoded = ascii(node.text);
        offset = operation * 40;
        data.setUint8(offset, 4);
        data.setUint8(offset + 1, node.bold ? 1 : 0);
        data.setUint16(offset + 2, node.id, true);
        data.setUint16(offset + 4, 1, true);
        data.setUint8(offset + 6, encoded.length);
        data.setUint32(offset + 8, node.color, true);
        bytes.set(encoded, offset + 12);
        operation += 1;
      }
    }
    if (LiteUI.commit(bytes, operationCount) !== 0) {
      throw new Error("LiteUI rejected the application transaction");
    }
  }
  const handlers = new Map();
  for (const node of ordered) {
    if (node.onClick !== undefined) handlers.set(node.id, node.onClick);
  }
  if (handlers.size !== 0) {
    LiteUI.onEvent((node, generation) => {
      if (generation !== 1) throw new Error("stale LiteUI event generation");
      const handler = handlers.get(node);
      if (handler === undefined) throw new Error("event targets an unsubscribed node");
      handler();
      commit(false);
    });
  }
  commit(true);
  globalThis.__liteosApplication = Object.freeze({
    abi: 1,
    framework: "solid-js",
    nodes: ordered.length,
    operations: operationCount,
  });
}

return Object.freeze({
  RIGHT: 1,
  BOTTOM: 2,
  STRETCH_WIDTH: 4,
  STRETCH_HEIGHT: 8,
  WINDOW: 1,
  TITLE_BAR: 2,
  CLOSE: 3,
  MINIMIZE: 4,
  MAXIMIZE: 5,
  RESTORE: 6,
  ACTION: 7,
  TEXT_GRID: 8,
  button,
  label,
  publish,
  rectangle,
  view,
});
})();
