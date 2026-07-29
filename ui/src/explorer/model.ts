import type { FsEntry } from "lite:fs";

// evdev keycodes for explorer keyboard shortcuts on the global onKeyDown
// path (focused inputs still capture their own keys first).
export const KEY_ESC = 1;
export const KEY_BACKSPACE = 14;
export const KEY_ENTER = 28;
export const KEY_A = 30;
export const KEY_X = 45;
export const KEY_C = 46;
export const KEY_V = 47;
export const KEY_F2 = 60;
export const KEY_DELETE = 111;
/** display-proto modifier mask bit for either Ctrl key. */
export const MOD_CONTROL = 2;

// Logical-pixel advances of the checked liteos-ui.a8p regular face at 11px
// (atlas pixel_size 22 ÷ deviceScaleFactor 2), extracted from the atlas itself
// — the same per-glyph `advance` values `font.rs measure()` sums, so the
// Rename box hugs its text instead of using a
// flat per-character guess. CJK and the U+FFFD fallback advance 11px.
const GLYPH_ADVANCE_11: Record<string, number> = {
  " ": 2.5, "!": 3.5, "\"": 5, "#": 6, "$": 6, "%": 10, "&": 7.5, "'": 3,
  "(": 3.5, ")": 3.5, "*": 5, "+": 6, ",": 3, "-": 4, ".": 3, "/": 4.5,
  "0": 6, "1": 6, "2": 6, "3": 6, "4": 6, "5": 6, "6": 6, "7": 6, "8": 6, "9": 6,
  ":": 3, ";": 3, "<": 6, "=": 6, ">": 6, "?": 5, "@": 10.5,
  "A": 6.5, "B": 7, "C": 7, "D": 7.5, "E": 6.5, "F": 6, "G": 7.5, "H": 8,
  "I": 3, "J": 6, "K": 7, "L": 6, "M": 9, "N": 8, "O": 8, "P": 7, "Q": 8,
  "R": 7, "S": 6.5, "T": 6.5, "U": 8, "V": 6.5, "W": 9.5, "X": 6.5, "Y": 6, "Z": 6.5,
  "[": 3.5, "\\": 4.5, "]": 3.5, "^": 6, "_": 6, "`": 6.5,
  "a": 6, "b": 7, "c": 5.5, "d": 7, "e": 6, "f": 3.5, "g": 6, "h": 6.5,
  "i": 3, "j": 3, "k": 6, "l": 3, "m": 10, "n": 6.5, "o": 6.5, "p": 7, "q": 7,
  "r": 4.5, "s": 5, "t": 4, "u": 6.5, "v": 5.5, "w": 9, "x": 5.5, "y": 5.5, "z": 5,
  "{": 3.5, "|": 3, "}": 3.5, "~": 6,
};

/** Logical-pixel width of `text` in the 11px UI font (real atlas advances). */
export function measureText11(text: string): number {
  let width = 0;
  for (const char of text) width += GLYPH_ADVANCE_11[char] ?? 11;
  return width;
}

/** Joins a directory path with a child name, keeping a single leading slash. */
export function joinPath(dir: string, name: string): string {
  return dir === "/" ? `/${name}` : `${dir}/${name}`;
}

/** Parent directory of an absolute path (`/` stays `/`). */
export function parentPath(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const cut = trimmed.lastIndexOf("/");
  return cut <= 0 ? "/" : trimmed.slice(0, cut);
}

/** Trailing name component of an absolute path (used to rebuild move targets). */
export function baseName(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const cut = trimmed.lastIndexOf("/");
  return cut < 0 ? trimmed : trimmed.slice(cut + 1);
}

export function formatSize(entry: FsEntry): string {
  if (entry.kind === "dir") return "";
  if (entry.kind === "symlink") return "";
  if (entry.size < 1024) return `${entry.size} B`;
  if (entry.size < 1024 * 1024) return `${Math.round(entry.size / 1024)} KB`;
  return `${Math.round(entry.size / (1024 * 1024))} MB`;
}

/** zh-CN date column (`2026/7/26 20:03`) from Unix seconds; "" for 0
 * (platform gave no timestamp), so unsupported entries show an empty cell
 * instead of a fake epoch date. */
export function formatDate(mtime: number): string {
  if (mtime <= 0) return "";
  const date = new Date(mtime * 1000);
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getFullYear()}/${date.getMonth() + 1}/${date.getDate()} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** en-US date column (`7/26/2026 8:03 PM`); "" for 0, same contract as
 * {@link formatDate}. */
export function formatDateEn(mtime: number): string {
  if (mtime <= 0) return "";
  const date = new Date(mtime * 1000);
  const hours = date.getHours();
  const hour12 = hours % 12 === 0 ? 12 : hours % 12;
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${date.getMonth() + 1}/${date.getDate()}/${date.getFullYear()} ${hour12}:${pad(date.getMinutes())} ${hours < 12 ? "AM" : "PM"}`;
}

/** Uppercased trailing extension, or "" when the name has no dotted suffix. */
export function extensionOf(name: string): string {
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toUpperCase() : "";
}

/** Per-app wording for the Type column / properties rows. */
export interface TypeLabels {
  folder: string;
  shortcut: string;
  file: string;
  /** Formats an extension-suffixed file, e.g. (ext) => `${ext} File`. */
  extensionFile: (extension: string) => string;
}

/** Human-readable Type column value, mirroring Explorer's "TXT File" phrasing. */
export function typeLabel(entry: FsEntry, labels: TypeLabels): string {
  if (entry.kind === "dir") return labels.folder;
  if (entry.kind === "symlink") return labels.shortcut;
  const ext = extensionOf(entry.name);
  return ext ? labels.extensionFile(ext) : labels.file;
}

/** A first free "New Folder" / "New Folder (2)" … name against existing entries. */
export function freshFolderName(taken: Set<string>, base: string): string {
  if (!taken.has(base)) return base;
  for (let index = 2; ; index += 1) {
    const candidate = `${base} (${index})`;
    if (!taken.has(candidate)) return candidate;
  }
}
