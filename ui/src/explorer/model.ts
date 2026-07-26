import type { FsEntry } from "lite:fs";

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
