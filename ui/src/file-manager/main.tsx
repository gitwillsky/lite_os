import React, { useCallback, useEffect, useRef, useState } from "react";
import { list, read } from "lite:fs";
import type { FsEntry } from "lite:fs";

/** Joins a directory path with a child name, keeping a single leading slash. */
function joinPath(dir: string, name: string): string {
  return dir === "/" ? `/${name}` : `${dir}/${name}`;
}

/** Parent directory of an absolute path (`/` stays `/`). */
function parentPath(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const cut = trimmed.lastIndexOf("/");
  return cut <= 0 ? "/" : trimmed.slice(0, cut);
}

function formatSize(entry: FsEntry): string {
  if (entry.kind === "dir") return "";
  if (entry.kind === "symlink") return "";
  if (entry.size < 1024) return `${entry.size} B`;
  if (entry.size < 1024 * 1024) return `${Math.round(entry.size / 1024)} KB`;
  return `${Math.round(entry.size / (1024 * 1024))} MB`;
}

/** Uppercased trailing extension, or "" when the name has no dotted suffix. */
function extensionOf(name: string): string {
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toUpperCase() : "";
}

/** Human-readable Type column value, mirroring Explorer's "TXT File" phrasing. */
function typeLabel(entry: FsEntry): string {
  if (entry.kind === "dir") return "File Folder";
  if (entry.kind === "symlink") return "Shortcut";
  const ext = extensionOf(entry.name);
  return ext ? `${ext} File` : "File";
}

/** 32px icon for the large-icon view (folders share one cached bitmap). */
function iconFor(entry: FsEntry): string {
  return entry.kind === "dir" || entry.kind === "symlink"
    ? "assets/folder.png"
    : "assets/file.png";
}

/** 16px icon for the details view and address bar. */
function iconFor16(entry: FsEntry): string {
  return entry.kind === "dir" || entry.kind === "symlink"
    ? "assets/folder-16.png"
    : "assets/file-16.png";
}

type ViewMode = "icons" | "details";

interface FileView {
  name: string;
  content: string;
  truncated: boolean;
  error?: string;
}

/** One interactive element's cached listeners; identities must stay stable
 * across renders because the compositor tracks hover by listener identity. */
/** One element's cached pointer listeners. Their identities must stay stable
 * across renders because the compositor tracks hover by listener identity;
 * click handlers are passed inline at the call site since only hover depends
 * on a stable identity. */
interface Handlers {
  onPointerEnter: () => void;
  onPointerLeave: () => void;
}

export default function FileManager() {
  const [path, setPath] = useState<string>("/");
  const [entries, setEntries] = useState<FsEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [fileView, setFileView] = useState<FileView | null>(null);
  const [viewMode, setViewMode] = useState<ViewMode>("icons");
  const [selected, setSelected] = useState<string | null>(null);
  const [hovered, setHovered] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Record<string, boolean>>({
    tasks: true,
    places: true,
  });

  // Re-list whenever the directory changes; a failed op returns a JSON error
  // object (never throws), but wrap defensively regardless.
  useEffect(() => {
    if (fileView) return;
    try {
      const result = list(path);
      if (result.error) {
        setEntries([]);
        setError(result.error);
      } else {
        const rows = (result.entries ?? []).slice().sort((a, b) => {
          if ((a.kind === "dir") !== (b.kind === "dir")) return a.kind === "dir" ? -1 : 1;
          return a.name < b.name ? -1 : 1;
        });
        setEntries(rows);
        setError(result.truncated ? "…more entries not shown" : null);
      }
    } catch {
      setEntries([]);
      setError("failed to read directory");
    }
  }, [path, fileView]);

  // Pointer handlers are cached by a namespaced key ("row:name", "tb:up",
  // "grp:places", …) so their identities — and thus the host listener ids the
  // compositor tracks hover by — stay stable across renders. Entry keys are
  // cleared on navigation since the previous directory's handlers are dead.
  const cache = useRef(new Map<string, Handlers>()).current;
  const bundle = useCallback((key: string): Handlers => {
    let handlers = cache.get(key);
    if (!handlers) {
      handlers = {
        onPointerEnter: () => setHovered(key),
        onPointerLeave: () => setHovered((current) => (current === key ? null : current)),
      };
      cache.set(key, handlers);
    }
    return handlers;
  }, [cache]);

  const goto = useCallback((next: string) => {
    cache.clear();
    setHovered(null);
    setSelected(null);
    setFileView(null);
    setPath(next);
  }, [cache]);

  const openEntry = useCallback((entry: FsEntry) => {
    const child = joinPath(path, entry.name);
    if (entry.kind === "dir" || entry.kind === "symlink") {
      goto(child);
      return;
    }
    try {
      const result = read(child);
      setFileView({
        name: entry.name,
        content: result.content ?? "",
        truncated: Boolean(result.truncated),
        error: result.error,
      });
    } catch {
      setFileView({ name: entry.name, content: "", truncated: false, error: "IO" });
    }
  }, [path, goto]);

  const cls = (base: string, key: string, extra?: string) =>
    `${base}${hovered === key ? ` ${base}--hover` : ""}${extra ? ` ${extra}` : ""}`;

  if (fileView) {
    return (
      <div className="fm">
        <div className="fm__addressbar">
          <div
            className={cls("fm__tb", "fv:back")}
            {...bundle("fv:back")}
            onClick={() => setFileView(null)}
          >
            <img className="fm__tb-icon" src="assets/tb-back.png"/>
            <span className="fm__tb-label">Back</span>
          </div>
          <div className="fm__addr-field">
            <img className="fm__addr-icon" src="assets/file-16.png"/>
            <span className="fm__addr-path">{joinPath(path, fileView.name)}</span>
          </div>
        </div>
        {fileView.error === "not-text"
          ? <div className="fm__note">Binary file - cannot display.</div>
          : fileView.error
            ? <div className="fm__note">Error: {fileView.error}</div>
            : <div className="fm__fileview"><span>{fileView.content}{fileView.truncated ? "\n… (truncated)" : ""}</span></div>}
      </div>
    );
  }

  const atRoot = path === "/";
  const menus = ["File", "Edit", "View", "Favorites", "Tools", "Help"];
  const toggleGroup = (id: string) =>
    setExpanded((current) => ({ ...current, [id]: !current[id] }));

  return (
    <div className="fm">
      <div className="fm__menubar">
        {menus.map((label) => (
          <span key={label} className={cls("fm__menu", `menu:${label}`)} {...bundle(`menu:${label}`)}>
            {label}
          </span>
        ))}
      </div>

      <div className="fm__toolbar">
        <div
          className={cls("fm__tb", "tb:back", atRoot ? "fm__tb--disabled" : undefined)}
          {...bundle("tb:back")}
          onClick={() => !atRoot && goto(parentPath(path))}
        >
          <img className="fm__tb-icon" src="assets/tb-back.png"/>
          <span className="fm__tb-label">Back</span>
        </div>
        <div className={cls("fm__tb", "tb:forward", "fm__tb--disabled")} {...bundle("tb:forward")}>
          <img className="fm__tb-icon" src="assets/tb-forward.png"/>
          <span className="fm__tb-label">Forward</span>
        </div>
        <div
          className={cls("fm__tb", "tb:up", atRoot ? "fm__tb--disabled" : undefined)}
          {...bundle("tb:up")}
          onClick={() => !atRoot && goto(parentPath(path))}
        >
          <img className="fm__tb-icon" src="assets/tb-up.png"/>
        </div>
        <div className="fm__tb-sep"/>
        <div className={cls("fm__tb", "tb:search")} {...bundle("tb:search")}>
          <img className="fm__tb-icon" src="assets/tb-search.png"/>
          <span className="fm__tb-label">Search</span>
        </div>
        <div className={cls("fm__tb", "tb:folders")} {...bundle("tb:folders")}>
          <img className="fm__tb-icon" src="assets/tb-folders.png"/>
          <span className="fm__tb-label">Folders</span>
        </div>
        <div
          className={cls("fm__tb", "tb:views")}
          {...bundle("tb:views")}
          onClick={() => setViewMode((m) => (m === "icons" ? "details" : "icons"))}
        >
          <img className="fm__tb-icon" src="assets/tb-views.png"/>
          <span className="fm__tb-label">Views</span>
        </div>
      </div>

      <div className="fm__addressbar">
        <span className="fm__addr-label">Address</span>
        <div className="fm__addr-field">
          <img className="fm__addr-icon" src="assets/folder-16.png"/>
          <span className="fm__addr-path">{path}</span>
          <span className="fm__addr-drop">{"∨"}</span>
        </div>
        <div className={cls("fm__go", "go")} {...bundle("go")}>
          <img className="fm__go-icon" src="assets/tb-forward.png"/>
          <span>Go</span>
        </div>
      </div>

      <div className="fm__body">
        <div className="fm__taskpane">
          <div className="fm__group">
            <div
              className="fm__group-head"
              {...bundle("grp:tasks")}
              onClick={() => toggleGroup("tasks")}
            >
              <span>File and Folder Tasks</span>
              <span className="fm__group-chev">{expanded.tasks ? "∧" : "∨"}</span>
            </div>
            {expanded.tasks && (
              <div className="fm__group-body">
                <span className={cls("fm__task-link", "task:newfolder")} {...bundle("task:newfolder")}>Make a new folder</span>
                <span className={cls("fm__task-link", "task:publish")} {...bundle("task:publish")}>Publish this folder to the Web</span>
                <span className={cls("fm__task-link", "task:share")} {...bundle("task:share")}>Share this folder</span>
              </div>
            )}
          </div>
          <div className="fm__group">
            <div
              className="fm__group-head"
              {...bundle("grp:places")}
              onClick={() => toggleGroup("places")}
            >
              <span>Other Places</span>
              <span className="fm__group-chev">{expanded.places ? "∧" : "∨"}</span>
            </div>
            {expanded.places && (
              <div className="fm__group-body">
                {!atRoot && (
                  <span
                    className={cls("fm__task-link", "place:up")}
                    {...bundle("place:up")}
                    onClick={() => goto(parentPath(path))}
                  >
                    {parentPath(path)}
                  </span>
                )}
                <span
                  className={cls("fm__task-link", "place:root")}
                  {...bundle("place:root")}
                  onClick={() => goto("/")}
                >
                  My Computer
                </span>
              </div>
            )}
          </div>
        </div>

        <div className="fm__view">
          {error && <div className="fm__note">{error}</div>}
          {viewMode === "icons" ? (
            <div className="fm__icons">
              {entries.map((entry) => (
                <div
                  key={entry.name}
                  className={cls("fm__icon", `row:${entry.name}`, selected === entry.name ? "fm__icon--sel" : undefined)}
                  {...bundle(`row:${entry.name}`)}
                  onClick={() => setSelected(entry.name)}
                  onDoubleClick={() => openEntry(entry)}
                >
                  <img className="fm__icon-img" src={iconFor(entry)}/>
                  <span className="fm__icon-label">{entry.name}</span>
                </div>
              ))}
            </div>
          ) : (
            <div className="fm__details">
              <div className="fm__dh">
                <span className="fm__dh-name">Name</span>
                <span className="fm__dh-size">Size</span>
                <span className="fm__dh-type">Type</span>
              </div>
              {entries.map((entry) => (
                <div
                  key={entry.name}
                  className={cls("fm__drow", `row:${entry.name}`, selected === entry.name ? "fm__drow--sel" : undefined)}
                  {...bundle(`row:${entry.name}`)}
                  onClick={() => setSelected(entry.name)}
                  onDoubleClick={() => openEntry(entry)}
                >
                  <img className="fm__drow-img" src={iconFor16(entry)}/>
                  <span className="fm__dc-name">{entry.name}</span>
                  <span className="fm__dc-size">{formatSize(entry)}</span>
                  <span className="fm__dc-type">{typeLabel(entry)}</span>
                </div>
              ))}
            </div>
          )}
        </div>
      </div>

      <div className="fm__status">
        <span>{entries.length} objects</span>
      </div>
    </div>
  );
}
