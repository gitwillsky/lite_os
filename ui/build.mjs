import { build } from "esbuild";
import { copyFile, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const output = join(root, "dist");
const checkOnly = process.argv.includes("--check");
const products = [
  ["desktop", "src/desktop/entry.tsx", "src/desktop/style.css"],
  ["terminal", "src/terminal/entry.tsx", "src/terminal/style.css"],
  ["file-manager", "src/file-manager/entry.tsx", "src/file-manager/style.css"],
];

const liteModules = {
  "lite:apps": `
    export const apps = () => JSON.parse(globalThis.__liteNative("apps.list", ""));
    export const launch = (id) => globalThis.__liteNative("apps.launch", id);
  `,
  "lite:desktop": `
    globalThis.liteDesktopSubscribe = (callback) => globalThis.__liteSubscribe("desktop", callback);
    export const surfaces = () => JSON.parse(globalThis.__liteNative("desktop.surfaces", ""));
    export const close = (id) => globalThis.__liteNative("desktop.close", String(id));
    export const focus = (id) => globalThis.__liteNative("desktop.focus", String(id));
    export const move = (id, x, y) => globalThis.__liteNative("desktop.move", id + ":" + x + ":" + y);
    export const beginMove = (id, serial, minX, minY, maxX, maxY) =>
      globalThis.__liteNative("desktop.move.begin", [id, serial, minX, minY, maxX, maxY].join(":"));
    export const configure = (id, width, height) => Number(globalThis.__liteNative("desktop.configure", id + ":" + width + ":" + height));
    export const shutdown = () => globalThis.__liteNative("desktop.shutdown", "");
    export const clock = () => Number(globalThis.__liteNative("time.clock", ""));
  `,
  "lite:terminal": `
    globalThis.liteTerminalSubscribe = (callback) => globalThis.__liteSubscribe("terminal", callback);
    export const connect = (argv) => JSON.parse(globalThis.__liteNative("terminal.connect", JSON.stringify(argv)));
    export const input = (event) => globalThis.__liteNative("terminal.input", JSON.stringify(event));
  `,
  "lite:fs": `
    export const list = (path) => JSON.parse(globalThis.__liteNative("fs.list", path));
    export const read = (path) => JSON.parse(globalThis.__liteNative("fs.read", path));
  `,
};

const liteModulePlugin = {
  name: "lite-system-modules",
  setup(buildContext) {
    buildContext.onResolve({ filter: /^lite:/ }, ({ path }) => ({ path, namespace: "lite" }));
    buildContext.onLoad({ filter: /.*/, namespace: "lite" }, ({ path }) => {
      if (!(path in liteModules)) throw new Error(`unknown LiteUI system module '${path}'`);
      return { contents: liteModules[path], loader: "js" };
    });
  },
};

const reactSystemPlugin = {
  name: "react-system-modules",
  setup(buildContext) {
    buildContext.onResolve({ filter: /^react(\/jsx-runtime)?$/ }, ({ path }) => ({ path, namespace: "react-system" }));
    buildContext.onLoad({ filter: /.*/, namespace: "react-system" }, ({ path }) => ({
      loader: "js",
      contents: path === "react"
        ? `
          const React = globalThis.__liteReact;
          export default React;
          export const useEffect = React.useEffect;
          export const useCallback = React.useCallback;
          export const useMemo = React.useMemo;
          export const useRef = React.useRef;
          export const useState = React.useState;
        `
        : `
          export const jsx = globalThis.__liteJsxRuntime.jsx;
          export const jsxs = globalThis.__liteJsxRuntime.jsxs;
          export const Fragment = globalThis.__liteJsxRuntime.Fragment;
        `,
    }));
  },
};

const properties = new Set([
  "align-items", "background", "background-image", "border", "border-bottom", "border-color",
  "border-bottom-color", "border-bottom-style", "border-bottom-width", "border-left",
  "border-left-color", "border-left-style", "border-left-width", "border-radius", "border-right",
  "border-right-color", "border-right-style", "border-right-width", "border-style", "border-top",
  "border-top-color", "border-top-style", "border-top-width", "border-width",
  "bottom", "box-shadow", "color", "display", "flex", "flex-direction",
  "font-family", "font-size", "font-style", "font-weight", "gap", "height", "justify-content",
  "left", "line-height", "margin", "margin-bottom", "margin-left", "margin-right", "margin-top",
  "max-height", "max-width", "min-height",
  "min-width", "opacity", "overflow", "overflow-x", "overflow-y", "padding", "pointer-events", "position",
  "padding-bottom", "padding-left", "padding-right", "padding-top", "right", "cursor", "text-align",
  "text-overflow", "text-shadow", "top", "white-space", "width", "z-index",
]);

function validateCss(path, source) {
  if (/@|::|\[|\]|\*/.test(source)) {
    throw new Error(`${relative(root, path)}: unsupported CSS selector or at-rule`);
  }
  for (const block of source.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    const selector = block[1].trim();
    if (!selector || selector.includes(",")) {
      throw new Error(`${relative(root, path)}: selectors must be explicit and singular`);
    }
    for (const declaration of block[2].split(";")) {
      const text = declaration.trim();
      if (!text) continue;
      const separator = text.indexOf(":");
      const property = text.slice(0, separator).trim();
      if (separator < 1 || (!property.startsWith("--") && !properties.has(property))) {
        throw new Error(`${relative(root, path)}: unsupported CSS property '${property}'`);
      }
    }
  }
}

if (!checkOnly) {
  await rm(output, { recursive: true, force: true });
  await mkdir(output, { recursive: true });
  await build({
    entryPoints: [join(root, "src/runtime/entry.ts")],
    outfile: join(output, "runtime.js"),
    bundle: true,
    format: "esm",
    platform: "neutral",
    target: "es2023",
    minifySyntax: true,
    minifyWhitespace: true,
    define: { "process.env.NODE_ENV": '"production"' },
    logLevel: "warning",
  });
}

for (const [id, entryName, styleName] of products) {
  const stylePath = join(root, styleName);
  const style = await readFile(stylePath, "utf8");
  validateCss(stylePath, style);
  if (checkOnly) continue;
  const directory = join(output, id);
  await mkdir(directory, { recursive: true });
  await build({
    entryPoints: [join(root, entryName)],
    outfile: join(directory, "main.js"),
    bundle: true,
    format: "esm",
    platform: "neutral",
    target: "es2023",
    jsx: "automatic",
    minifySyntax: true,
    minifyWhitespace: true,
    define: { "process.env.NODE_ENV": '"production"' },
    plugins: [liteModulePlugin, reactSystemPlugin],
    logLevel: "warning",
  });
  await writeFile(join(directory, "style.css"), style);
  const assets = join(directory, "assets");
  await mkdir(assets, { recursive: true });
  if (id === "desktop") {
    await copyFile(join(root, "../assets/sprites-src/icon-computer.png"), join(assets, "computer.png"));
    await copyFile(join(root, "../assets/sprites-src/icon-documents.png"), join(assets, "documents.png"));
    await copyFile(join(root, "../assets/sprites-src/icon-trash.png"), join(assets, "trash.png"));
    await copyFile(join(root, "../assets/sprites-src/icon-speaker.png"), join(assets, "speaker.png"));
  }
  await copyFile(join(root, "../assets/sprites-src/icon-terminal.png"), join(assets, "terminal.png"));
  if (id === "file-manager") {
    await copyFile(join(root, "../assets/sprites-src/icon-computer.png"), join(assets, "computer.png"));
  }
  if (id !== "desktop") {
    await copyFile(join(root, `src/${id}/app.json`), join(directory, "app.json"));
  }
}
