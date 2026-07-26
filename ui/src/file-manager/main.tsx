import React, { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { list, mkdir, remove, rename, copy } from "lite:fs";
import type { FsEntry } from "lite:fs";
import { ContextMenu } from "../design-system/context-menu.tsx";

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

/** Trailing name component of an absolute path (used to rebuild move targets). */
function baseName(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const cut = trimmed.lastIndexOf("/");
  return cut < 0 ? trimmed : trimmed.slice(cut + 1);
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

/** A first free "New Folder" / "New Folder (2)" … name against existing entries. */
function freshFolderName(taken: Set<string>): string {
  if (!taken.has("New Folder")) return "New Folder";
  for (let index = 2; ; index += 1) {
    const candidate = `New Folder (${index})`;
    if (!taken.has(candidate)) return candidate;
  }
}

type ViewMode = "icons" | "details";

/** A cut/copied entry awaiting Paste; `mode` decides move vs copy. */
interface Clipboard {
  mode: "cut" | "copy";
  path: string;
}

/** An open context menu: viewport-local position and its rows. */
interface MenuState {
  x: number;
  y: number;
  items: { id: string; label: string; onSelect?: () => void }[];
}

/** One element's cached pointer listeners. Their identities must stay stable
 * across renders because the compositor tracks hover by listener identity;
 * click handlers are passed inline at the call site since only hover depends
 * on a stable identity. */
interface Handlers {
  onPointerEnter: () => void;
  onPointerLeave: () => void;
}

// evdev keycodes delivered on the focused input's onKeyDown for commit/cancel.
const KEY_ESC = 1;
const KEY_ENTER = 28;
// Fixed menubar geometry: dropdowns open just under the clicked label. The
// menubar is a fixed row, so a per-label x offset and a constant y suffice.
const MENUBAR_TOP = 40;
const MENU_LABEL_X = 8;
const MENU_LABEL_STRIDE = 42;

export default function FileManager() {
  const [path, setPath] = useState<string>("/");
  // Visited-path stack for real Back/Forward (Up stays "parent", distinct from
  // Back). `historyIndex` points at the current entry; navigating truncates any
  // forward tail, matching a browser/Explorer history.
  const [history, setHistory] = useState<string[]>(["/"]);
  const [historyIndex, setHistoryIndex] = useState(0);
  const [entries, setEntries] = useState<FsEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [viewMode, setViewMode] = useState<ViewMode>("icons");
  const [selected, setSelected] = useState<string | null>(null);
  const [hovered, setHovered] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Record<string, boolean>>({
    tasks: true,
    places: true,
  });
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [clipboard, setClipboard] = useState<Clipboard | null>(null);
  // The entry name being renamed and its editable draft, or null when idle.
  const [renaming, setRenaming] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  // The address bar's editable draft when the path field is focused, else null.
  const [addressDraft, setAddressDraft] = useState<string | null>(null);

  // Re-list the current directory. Extracted from the mount effect so every
  // write (New Folder / Delete / Paste / Rename) can refresh the view.
  const refresh = useCallback(() => {
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
  }, [path]);

  useEffect(() => { refresh(); }, [refresh]);

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

  const closeMenu = useCallback(() => setMenu(null), []);
  const openMenu = useCallback((x: number, y: number, items: MenuState["items"]) => {
    setMenu({ x, y, items });
  }, []);

  // Clear the per-directory hover/selection state and dismiss transient popups.
  const resetView = useCallback(() => {
    cache.clear();
    setHovered(null);
    setSelected(null);
    setMenu(null);
    setRenaming(null);
    setAddressDraft(null);
  }, [cache]);

  // Navigate to a new directory, pushing history (truncating the forward tail).
  const navigate = useCallback((next: string) => {
    resetView();
    setHistory((stack) => {
      const kept = stack.slice(0, historyIndex + 1);
      kept.push(next);
      setHistoryIndex(kept.length - 1);
      return kept;
    });
    setPath(next);
  }, [historyIndex, resetView]);

  const back = useCallback(() => {
    if (historyIndex <= 0) return;
    const index = historyIndex - 1;
    resetView();
    setHistoryIndex(index);
    setPath(history[index]);
  }, [history, historyIndex, resetView]);

  const forward = useCallback(() => {
    if (historyIndex >= history.length - 1) return;
    const index = historyIndex + 1;
    resetView();
    setHistoryIndex(index);
    setPath(history[index]);
  }, [history, historyIndex, resetView]);

  const up = useCallback(() => navigate(parentPath(path)), [navigate, path]);

  // Double-click opens a folder (navigate in). Files have no associated-program
  // system here, so double-clicking a file only keeps it selected — matching XP,
  // where opening a file is delegated to its handler rather than the shell.
  const openEntry = useCallback((entry: FsEntry) => {
    if (entry.kind === "dir" || entry.kind === "symlink") {
      navigate(joinPath(path, entry.name));
    } else {
      setSelected(entry.name);
    }
  }, [path, navigate]);

  // Surface a native mutation's error code in the note banner, else refresh.
  const applyResult = useCallback((result: { error?: string }) => {
    if (result.error) setError(result.error);
    else refresh();
  }, [refresh]);

  const newFolder = useCallback(() => {
    const taken = new Set(entries.map((entry) => entry.name));
    applyResult(mkdir(joinPath(path, freshFolderName(taken))));
  }, [entries, path, applyResult]);

  const deleteEntry = useCallback((entry: FsEntry) => {
    // A folder is removed with its contents (Explorer sends the whole subtree);
    // files/symlinks are unlinked. The native side gates recursion explicitly.
    applyResult(remove(joinPath(path, entry.name), entry.kind === "dir"));
    setSelected((current) => (current === entry.name ? null : current));
  }, [path, applyResult]);

  const paste = useCallback(() => {
    if (!clipboard) return;
    const target = joinPath(path, baseName(clipboard.path));
    const result = clipboard.mode === "cut"
      ? rename(clipboard.path, target)
      : copy(clipboard.path, target);
    if (clipboard.mode === "cut" && !result.error) setClipboard(null);
    applyResult(result);
  }, [clipboard, path, applyResult]);

  const commitRename = useCallback(() => {
    const original = renaming;
    setRenaming(null);
    if (!original || !renameDraft || renameDraft === original) return;
    applyResult(rename(joinPath(path, original), joinPath(path, renameDraft)));
  }, [renaming, renameDraft, path, applyResult]);

  const cls = (base: string, key: string, extra?: string) =>
    `${base}${hovered === key ? ` ${base}--hover` : ""}${extra ? ` ${extra}` : ""}`;

  const atRoot = path === "/";
  const canBack = historyIndex > 0;
  const canForward = historyIndex < history.length - 1;
  const menus = ["File", "Edit", "View", "Favorites", "Tools", "Help"];
  const toggleGroup = (id: string) =>
    setExpanded((current) => ({ ...current, [id]: !current[id] }));

  // Row context menu: Open/Cut/Copy/Delete/Rename operate on one entry.
  const rowMenu = useCallback((entry: FsEntry): MenuState["items"] => [
    { id: "open", label: "Open", onSelect: () => openEntry(entry) },
    { id: "cut", label: "Cut", onSelect: () => setClipboard({ mode: "cut", path: joinPath(path, entry.name) }) },
    { id: "copy", label: "Copy", onSelect: () => setClipboard({ mode: "copy", path: joinPath(path, entry.name) }) },
    { id: "delete", label: "Delete", onSelect: () => deleteEntry(entry) },
    { id: "rename", label: "Rename", onSelect: () => { setRenaming(entry.name); setRenameDraft(entry.name); } },
  ], [openEntry, path, deleteEntry]);

  // Empty-area context menu: New Folder, Paste (only with a clipboard), Refresh.
  const emptyMenu = useCallback((): MenuState["items"] => {
    const items: MenuState["items"] = [{ id: "new", label: "New Folder", onSelect: newFolder }];
    if (clipboard) items.push({ id: "paste", label: "Paste", onSelect: paste });
    items.push({ id: "refresh", label: "Refresh", onSelect: refresh });
    return items;
  }, [newFolder, clipboard, paste, refresh]);

  // Menubar dropdowns. Each label opens a ContextMenu just below it.
  const openMenubar = useCallback((label: string, index: number) => {
    const x = MENU_LABEL_X + index * MENU_LABEL_STRIDE;
    const items: Record<string, MenuState["items"]> = {
      File: [
        { id: "new", label: "New Folder", onSelect: newFolder },
        { id: "refresh", label: "Refresh", onSelect: refresh },
      ],
      Edit: [
        { id: "cut", label: "Cut", onSelect: () => selected && setClipboard({ mode: "cut", path: joinPath(path, selected) }) },
        { id: "copy", label: "Copy", onSelect: () => selected && setClipboard({ mode: "copy", path: joinPath(path, selected) }) },
        { id: "paste", label: "Paste", onSelect: paste },
      ],
      View: [
        { id: "icons", label: "Icons", onSelect: () => setViewMode("icons") },
        { id: "details", label: "Details", onSelect: () => setViewMode("details") },
        { id: "refresh", label: "Refresh", onSelect: refresh },
      ],
    };
    if (items[label]) openMenu(x, MENUBAR_TOP, items[label]);
  }, [newFolder, refresh, selected, path, paste, openMenu]);

  // Address caret: a dropdown of ancestor directories, each navigable.
  const ancestors = useMemo(() => {
    const parts = path.split("/").filter(Boolean);
    const list: MenuState["items"] = [{ id: "/", label: "My Computer", onSelect: () => navigate("/") }];
    let acc = "";
    for (const part of parts) {
      acc = `${acc}/${part}`;
      const full = acc;
      list.push({ id: full, label: full, onSelect: () => navigate(full) });
    }
    return list;
  }, [path, navigate]);

  return (
    <div className="fm" onClick={() => { setMenu(null); }}>
      <div className="fm__menubar">
        {menus.map((label, index) => (
          <span
            key={label}
            className={cls("fm__menu", `menu:${label}`)}
            {...bundle(`menu:${label}`)}
            onClick={() => openMenubar(label, index)}
          >
            {label}
          </span>
        ))}
      </div>

      <div className="fm__toolbar">
        <div
          className={cls("fm__tb", "tb:back", canBack ? undefined : "fm__tb--disabled")}
          {...bundle("tb:back")}
          onClick={() => canBack && back()}
        >
          <img className="fm__tb-icon" src="assets/tb-back.png"/>
          <span className="fm__tb-label">Back</span>
        </div>
        <div
          className={cls("fm__tb", "tb:forward", canForward ? undefined : "fm__tb--disabled")}
          {...bundle("tb:forward")}
          onClick={() => canForward && forward()}
        >
          <img className="fm__tb-icon" src="assets/tb-forward.png"/>
          <span className="fm__tb-label">Forward</span>
        </div>
        <div
          className={cls("fm__tb", "tb:up", atRoot ? "fm__tb--disabled" : undefined)}
          {...bundle("tb:up")}
          onClick={() => !atRoot && up()}
        >
          <img className="fm__tb-icon" src="assets/tb-up.png"/>
        </div>
        <div className="fm__tb-sep"/>
        <div
          className={cls("fm__tb", "tb:folders")}
          {...bundle("tb:folders")}
          onClick={newFolder}
        >
          <img className="fm__tb-icon" src="assets/tb-folders.png"/>
          <span className="fm__tb-label">New Folder</span>
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
        <div className="fm__addr-field" onClick={() => setAddressDraft(path)}>
          <img className="fm__addr-icon" src="assets/folder-16.png"/>
          {addressDraft === null ? (
            <span className="fm__addr-path">{path}</span>
          ) : (
            <input
              className="fm__addr-input"
              value={addressDraft}
              onInput={(event) => setAddressDraft((event as unknown as { value: string }).value)}
              onKeyDown={(event) => {
                const key = event as unknown as { code: number; value: number };
                if (key.value === 0) return;
                if (key.code === KEY_ENTER) navigate(addressDraft ?? path);
                else if (key.code === KEY_ESC) setAddressDraft(null);
              }}
            />
          )}
          <span
            className="fm__addr-drop"
            {...bundle("addr:drop")}
            onClick={() => openMenu(8, 64, ancestors)}
          >
            <img className="fm__caret" src="assets/caret-down.png"/>
          </span>
        </div>
        <div className={cls("fm__go", "go")} {...bundle("go")} onClick={refresh}>
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
              <span className="fm__group-chev"><img className="fm__chev" src={expanded.tasks ? "assets/chev-up.png" : "assets/chev-down.png"}/></span>
            </div>
            {expanded.tasks && (
              <div className="fm__group-body">
                <span className={cls("fm__task-link", "task:newfolder")} {...bundle("task:newfolder")} onClick={newFolder}>Make a new folder</span>
                {selected && <span className={cls("fm__task-link", "task:rename")} {...bundle("task:rename")} onClick={() => { setRenaming(selected); setRenameDraft(selected); }}>Rename this item</span>}
                {selected && <span className={cls("fm__task-link", "task:delete")} {...bundle("task:delete")} onClick={() => { const entry = entries.find((e) => e.name === selected); if (entry) deleteEntry(entry); }}>Delete this item</span>}
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
              <span className="fm__group-chev"><img className="fm__chev" src={expanded.places ? "assets/chev-up.png" : "assets/chev-down.png"}/></span>
            </div>
            {expanded.places && (
              <div className="fm__group-body">
                {!atRoot && (
                  <span
                    className={cls("fm__task-link", "place:up")}
                    {...bundle("place:up")}
                    onClick={up}
                  >
                    {parentPath(path)}
                  </span>
                )}
                <span
                  className={cls("fm__task-link", "place:root")}
                  {...bundle("place:root")}
                  onClick={() => navigate("/")}
                >
                  My Computer
                </span>
              </div>
            )}
          </div>
        </div>

        <div
          className="fm__view"
          onContextMenu={(rawEvent) => {
            const event = rawEvent as unknown as { x: number; y: number };
            openMenu(event.x, event.y, emptyMenu());
          }}
        >
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
                  onContextMenu={(rawEvent) => {
                    const event = rawEvent as unknown as { x: number; y: number };
                    setSelected(entry.name);
                    openMenu(event.x, event.y, rowMenu(entry));
                  }}
                >
                  <img className="fm__icon-img" src={iconFor(entry)}/>
                  {renaming === entry.name ? (
                    <input
                      className="fm__rename"
                      value={renameDraft}
                      onInput={(event) => setRenameDraft((event as unknown as { value: string }).value)}
                      onKeyDown={(event) => {
                        const key = event as unknown as { code: number; value: number };
                        if (key.value === 0) return;
                        if (key.code === KEY_ENTER) commitRename();
                        else if (key.code === KEY_ESC) setRenaming(null);
                      }}
                    />
                  ) : (
                    <span className="fm__icon-label">{entry.name}</span>
                  )}
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
                  onContextMenu={(rawEvent) => {
                    const event = rawEvent as unknown as { x: number; y: number };
                    setSelected(entry.name);
                    openMenu(event.x, event.y, rowMenu(entry));
                  }}
                >
                  <img className="fm__drow-img" src={iconFor16(entry)}/>
                  {renaming === entry.name ? (
                    <input
                      className="fm__rename"
                      value={renameDraft}
                      onInput={(event) => setRenameDraft((event as unknown as { value: string }).value)}
                      onKeyDown={(event) => {
                        const key = event as unknown as { code: number; value: number };
                        if (key.value === 0) return;
                        if (key.code === KEY_ENTER) commitRename();
                        else if (key.code === KEY_ESC) setRenaming(null);
                      }}
                    />
                  ) : (
                    <span className="fm__dc-name">{entry.name}</span>
                  )}
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

      {menu && <ContextMenu x={menu.x} y={menu.y} items={menu.items} onClose={closeMenu}/>}
    </div>
  );
}
