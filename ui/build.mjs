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
  ["my-computer", "src/my-computer/entry.tsx", "src/my-computer/style.css"],
  ["music-player", "src/music-player/entry.tsx", "src/music-player/style.css"],
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
    export const setAccelerators = (chords) => globalThis.__liteNative("desktop.accelerators.set", JSON.stringify(chords));
    export const shutdown = () => globalThis.__liteNative("desktop.shutdown", "");
    export const clock = () => Number(globalThis.__liteNative("time.clock", ""));
  `,
  "lite:terminal": `
    globalThis.liteTerminalSubscribe = (callback) => globalThis.__liteSubscribe("terminal", callback);
    export const connect = (argv) => JSON.parse(globalThis.__liteNative("terminal.connect", JSON.stringify(argv)));
    export const input = (event) => globalThis.__liteNative("terminal.input", JSON.stringify(event));
    export const paste = (text) => globalThis.__liteNative("terminal.paste", text);
  `,
  "lite:audio-system": `
    export const subscribe = (callback) => globalThis.__liteSubscribe("audio-system", callback);
    export const getState = () => globalThis.__liteNative("audio-system.get", "");
    export const setVolume = (percent) => globalThis.__liteNative("audio-system.volume", String(percent));
    export const setMuted = (muted) => globalThis.__liteNative("audio-system.muted", String(muted));
  `,
  "lite:fs": `
    export const list = (path) => JSON.parse(globalThis.__liteNative("fs.list", path));
    export const read = (path) => JSON.parse(globalThis.__liteNative("fs.read", path));
    export const open = (path) => globalThis.__liteFile(JSON.parse(globalThis.__liteNative("fs.open", path)));
    export const mkdir = (path) => JSON.parse(globalThis.__liteNative("fs.mkdir", path));
    export const remove = (path, recursive = false) => JSON.parse(globalThis.__liteNative("fs.remove", JSON.stringify({ path, recursive })));
    export const rename = (from, to) => JSON.parse(globalThis.__liteNative("fs.rename", JSON.stringify({ from, to })));
    export const copy = (from, to) => JSON.parse(globalThis.__liteNative("fs.copy", JSON.stringify({ from, to })));
  `,
};

const liteModulePlugin = (product) => ({
  name: "lite-system-modules",
  setup(buildContext) {
    buildContext.onResolve({ filter: /^lite:/ }, ({ path }) => ({ path, namespace: "lite" }));
    buildContext.onLoad({ filter: /.*/, namespace: "lite" }, ({ path }) => {
      if (!(path in liteModules)) throw new Error(`unknown LiteUI system module '${path}'`);
      if (path === "lite:audio-system" && product !== "desktop") {
        throw new Error("lite:audio-system is available only to the desktop bundle");
      }
      return { contents: liteModules[path], loader: "js" };
    });
  },
});

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
  "accent-color", "align-items", "background", "background-color", "background-image", "background-position",
  "background-repeat", "background-size", "border", "border-bottom", "border-color",
  "border-bottom-color", "border-bottom-style", "border-bottom-width", "border-left",
  "border-left-color", "border-left-style", "border-left-width", "border-radius", "border-right",
  "border-right-color", "border-right-style", "border-right-width", "border-style", "border-top",
  "border-top-color", "border-top-style", "border-top-width", "border-width",
  "animation", "backdrop-filter", "bottom", "box-shadow", "box-sizing", "color", "display", "flex", "flex-basis",
  "flex-direction", "flex-grow", "flex-shrink", "flex-wrap",
  "font-family", "font-size", "font-style", "font-weight", "gap", "height", "justify-content",
  "left", "line-height", "margin", "margin-bottom", "margin-left", "margin-right", "margin-top",
  "max-height", "max-width", "min-height",
  "min-width", "opacity", "overflow", "overflow-x", "overflow-y", "padding", "pointer-events", "position",
  "padding-bottom", "padding-left", "padding-right", "padding-top", "right", "cursor", "text-align",
  "text-overflow", "text-shadow", "top", "transform", "transition", "white-space", "width", "z-index",
]);

function validateCss(path, source) {
  const location = relative(root, path);
  const declarations = (body) => {
    for (const declaration of body.split(";")) {
      const text = declaration.trim();
      if (!text) continue;
      const separator = text.indexOf(":");
      const property = text.slice(0, separator).trim();
      if (separator < 1 || (!property.startsWith("--") && !properties.has(property))) {
        throw new Error(`${location}: unsupported CSS property '${property}'`);
      }
    }
  };
  const blocks = (text) => {
    const output = [];
    let cursor = 0;
    while (cursor < text.length) {
      while (cursor < text.length && /\s/.test(text[cursor])) cursor += 1;
      if (cursor === text.length) break;
      const open = text.indexOf("{", cursor);
      if (open < 0) throw new Error(`${location}: CSS contains trailing input`);
      const header = text.slice(cursor, open).trim();
      let depth = 1;
      let close = open + 1;
      for (; close < text.length && depth > 0; close += 1) {
        if (text[close] === "{") depth += 1;
        if (text[close] === "}") depth -= 1;
      }
      if (depth !== 0) throw new Error(`${location}: CSS block is unterminated`);
      output.push([header, text.slice(open + 1, close - 1)]);
      cursor = close;
    }
    return output;
  };
  const validate = (text, context = "rules") => {
    for (const [header, body] of blocks(text)) {
      if (header.startsWith("@media ")) {
        if (!["(prefers-reduced-motion: reduce)", "(prefers-reduced-motion: no-preference)"]
          .includes(header.slice(7).trim())) {
          throw new Error(`${location}: unsupported media query '${header.slice(7).trim()}'`);
        }
        validate(body);
      } else if (header.startsWith("@keyframes ")) {
        if (!header.slice(11).trim()) throw new Error(`${location}: @keyframes requires a name`);
        validate(body, "keyframes");
      } else if (context === "keyframes") {
        if (!header.split(",").every((selector) =>
          /^(from|to|\d+(\.\d+)?%)$/.test(selector.trim()))) {
          throw new Error(`${location}: invalid keyframe selector '${header}'`);
        }
        declarations(body);
      } else {
        const selectors = header.split(",").map((selector) => selector.trim());
        if (selectors.some((selector) => !selector || /::|\[|\]|\*/.test(selector))) {
          throw new Error(`${location}: selector lists must contain explicit supported selectors`);
        }
        declarations(body);
      }
    }
  };
  validate(source);
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

// design-system 独占 Aurora token 与系统组件样式。构建期 prepend 到每个 app
// 的业务布局之前；CSS custom properties 在运行时级联，因此颜色、圆角、阴影与
// 控件状态始终只有这一份 owner。
const sharedTheme = await readFile(join(root, "src/design-system/theme.css"), "utf8");

for (const [id, entryName, styleName] of products) {
  const stylePath = join(root, styleName);
  const appStyle = await readFile(stylePath, "utf8");
  // 共享主题在前、app 自有样式在后，后者可覆盖同名类。整体过验证器。
  const style = `${sharedTheme}\n${appStyle}`;
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
    plugins: [liteModulePlugin(id), reactSystemPlugin],
    logLevel: "warning",
  });
  await writeFile(join(directory, "style.css"), style);
  const assets = join(directory, "assets");
  await mkdir(assets, { recursive: true });
  if (id === "desktop") {
    for (const name of ["liteos.png", "files.png", "terminal.png", "monitor.png", "package.png", "settings.png", "wallpaper.png"]) {
      await copyFile(join(root, `../assets/aurora/${name}`), join(assets, name));
    }
    // Command Center recent list reuses the generic file sprite.
    await copyFile(join(root, "../assets/sprites-src/file.png"), join(assets, "file.png"));
    // Aurora status / quick-settings glyphs (topbar, System Center, Command Center).
    for (const name of [
      "wifi.png", "network.png", "battery.png", "battery-lg.png",
      "bluetooth.png", "night-light.png", "do-not-disturb.png", "airplane.png", "focus.png",
      "brightness.png", "volume.png", "speakers.png",
      "microphone.png", "all-apps.png", "lock.png", "sleep.png", "restart.png", "power.png",
    ]) {
      await copyFile(join(root, `../assets/aurora-glyphs-src/${name}`), join(assets, name));
    }
    await copyFile(join(root, "../assets/splash/aurora-background.png"), join(assets, "aurora-background.png"));
    await copyFile(join(root, "../assets/splash/aurora-logo.png"), join(assets, "aurora-logo.png"));
  }
  if (id === "file-manager") {
    await copyFile(join(root, "../assets/aurora/files.png"), join(assets, "files.png"));
    await copyFile(join(root, "../assets/aurora/view-grid.png"), join(assets, "view-grid.png"));
    // Nav glyphs come from the Codex-generated aurora-glyphs set (the old
    // assets/aurora/nav-*.png were near-empty and invisible on the dark toolbar).
    for (const name of ["nav-back.png", "nav-forward.png", "nav-up.png", "nav-home.png"]) {
      await copyFile(join(root, `../assets/aurora-glyphs-src/${name}`), join(assets, name));
    }
    await copyFile(join(root, "../assets/sprites-src/file.png"), join(assets, "file.png"));
    await copyFile(join(root, "../assets/sprites-src/folder-16.png"), join(assets, "folder-16.png"));
    await copyFile(join(root, "../assets/sprites-src/file-16.png"), join(assets, "file-16.png"));
    await copyFile(join(root, "../assets/sprites-src/caret-down.png"), join(assets, "caret-down.png"));
  }
  if (id === "my-computer") {
    await copyFile(join(root, "../assets/aurora/package.png"), join(assets, "package.png"));
    await copyFile(join(root, "../assets/aurora/files.png"), join(assets, "files.png"));
    for (const name of ["nav-back.png", "nav-forward.png", "nav-up.png", "view-grid.png"]) {
      await copyFile(join(root, `../assets/aurora/${name}`), join(assets, name));
    }
    await copyFile(join(root, "../assets/sprites-src/icon-drive.png"), join(assets, "drive.png"));
    await copyFile(join(root, "../assets/sprites-src/icon-drive-16.png"), join(assets, "drive-16.png"));
    await copyFile(join(root, "../assets/sprites-src/file.png"), join(assets, "file.png"));
    await copyFile(join(root, "../assets/sprites-src/folder-16.png"), join(assets, "folder-16.png"));
    await copyFile(join(root, "../assets/sprites-src/file-16.png"), join(assets, "file-16.png"));
    await copyFile(join(root, "../assets/sprites-src/chev-up.png"), join(assets, "chev-up.png"));
    await copyFile(join(root, "../assets/sprites-src/chev-down.png"), join(assets, "chev-down.png"));
    await copyFile(join(root, "../assets/sprites-src/caret-down.png"), join(assets, "caret-down.png"));
  }
  if (id === "music-player") {
    await copyFile(join(root, "../assets/aurora/monitor.png"), join(assets, "monitor.png"));
    await copyFile(join(root, "../assets/sprites-src/folder.png"), join(assets, "folder.png"));
    await copyFile(join(root, "../assets/sprites-src/file-16.png"), join(assets, "file-16.png"));
    await copyFile(join(root, "../assets/music/跟太阳系说再见/cover.png"), join(assets, "solar-system-cover.png"));
  }
  if (id !== "desktop") {
    await copyFile(join(root, `src/${id}/app.json`), join(directory, "app.json"));
  }
}
