import React, { useCallback, useMemo, useState } from "react";
import { capacity, list } from "lite:fs";
import type { FsEntry } from "lite:fs";
import { ContextMenu } from "../design-system/context-menu.tsx";
import {
  SearchField,
  Sidebar,
  SidebarItem,
  ViewSwitch,
} from "../design-system/controls.tsx";
import type { MenuItem } from "../design-system/controls.tsx";
import {
  KEY_A, KEY_BACKSPACE, KEY_C, KEY_DELETE, KEY_ESC, KEY_ENTER, KEY_F2, KEY_V, KEY_X,
  MOD_CONTROL,
  formatDateEn,
  formatRecent,
  joinPath,
  parentPath,
  typeLabel,
} from "../explorer/model.ts";
import type { TypeLabels } from "../explorer/model.ts";
import { useBrowser } from "../explorer/use-browser.ts";
import type { SortColumn } from "../explorer/use-browser.ts";
import {
  FolderView,
  blankMenu,
  entryMenu,
} from "../explorer/components.tsx";
import {
  CannotOpenDialog,
  TextViewer,
  readTextFile,
} from "../explorer/dialogs.tsx";
import type { FileView } from "../explorer/dialogs.tsx";

/** An open context menu: viewport-local position and its rows. */
interface MenuState {
  x: number;
  y: number;
  items: MenuItem[];
}

/** The one open file-result modal, or null. */
type DialogState =
  | { kind: "viewer"; view: FileView }
  | { kind: "cannot-open"; name: string }
  | null;

const TYPE_LABELS: TypeLabels = {
  folder: "File Folder",
  shortcut: "Shortcut",
  file: "File",
  extensionFile: (extension) => `${extension} File`,
};

/** 48px icon for the large-icon view (folders share one cached bitmap). */
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

/** Formats filesystem byte counts for the compact storage summary. */
function formatCapacity(bytes: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(unit === 0 || value >= 10 ? 0 : 1)} ${units[unit]}`;
}

/** One "Recent files" row: a real file plus the folder it lives in. */
interface RecentFile {
  name: string;
  path: string;
  mtime: number;
}

/** Collect real files one level under `root`'s folders (plus root's own
 * files), newest first. Powers the Home "Recent files" list from actual fs
 * data rather than a mock — synchronous lite:fs listing, capped at `limit`. */
function collectRecentFiles(root: string, limit: number): RecentFile[] {
  const files: RecentFile[] = [];
  const scan = (dir: string) => {
    const result = list(dir);
    for (const entry of result.entries ?? []) {
      if (entry.name.startsWith(".")) continue;
      if (entry.kind === "file") {
        files.push({ name: entry.name, path: joinPath(dir, entry.name), mtime: entry.mtime });
      }
    }
  };
  scan(root);
  for (const entry of list(root).entries ?? []) {
    if (entry.kind === "dir" && !entry.name.startsWith(".")) scan(joinPath(root, entry.name));
  }
  return files.sort((a, b) => b.mtime - a.mtime).slice(0, limit);
}

export default function FileManager() {
  const [dialog, setDialog] = useState<DialogState>(null);
  const closeDialog = useCallback(() => setDialog(null), []);

  const openFile = useCallback((path: string, entry: FsEntry) => {
    const result = readTextFile(path, entry);
    if ("view" in result) setDialog({ kind: "viewer", view: result.view });
    else setDialog({ kind: "cannot-open", name: entry.name });
  }, []);

  const browser = useBrowser("/root", { typeLabels: TYPE_LABELS, onOpenFile: openFile });
  const { path, entries, error, viewMode, setViewMode, selected } = browser;
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [query, setQuery] = useState("");
  // One clock read for the Home "Recent files" relative stamps. Snapshotting
  // once (not per render) keeps the list stable and avoids an argless
  // new Date() during render; refreshing on navigation into Home is enough.
  const [now] = useState(() => new Date());

  const closeMenu = useCallback(() => setMenu(null), []);
  const openMenu = useCallback((x: number, y: number, items: MenuItem[]) => {
    setMenu({ x, y, items });
  }, []);

  const atRoot = path === "/root";
  const selectedEntries = useMemo(
    () => entries.filter((entry) => selected.includes(entry.name)),
    [entries, selected],
  );
  const focusedEntry = selectedEntries.at(-1) ?? null;
  const newFolder = useCallback(() => browser.newFolder("New Folder"), [browser]);

  // Row context menu: Open/Cut/Copy/Delete/Rename operate on the selection
  // the clicked row belongs to.
  const rowMenu = useCallback((entry: FsEntry): MenuItem[] => entryMenu(
    { open: "Open", cut: "Cut", copy: "Copy", delete: "Delete", rename: "Rename" },
    {
      onOpen: () => browser.openEntry(entry),
      onCut: () => browser.clipboardFromSelection("cut", selectedEntries),
      onCopy: () => browser.clipboardFromSelection("copy", selectedEntries),
      onDelete: () => browser.deleteSelected(selectedEntries),
      onRename: () => browser.beginRename(entry.name),
    },
  ), [browser, selectedEntries]);

  // Empty-area context menu: New Folder, Paste (only with a clipboard), Refresh.
  const emptyMenu = useCallback((): MenuItem[] => blankMenu(
    { newFolder: "New Folder", paste: "Paste", refresh: "Refresh" },
    {
      onNewFolder: newFolder,
      onPaste: browser.clipboard ? browser.paste : undefined,
      onRefresh: browser.refresh,
    },
  ), [newFolder, browser]);

  // Explorer keyboard map on the global onKeyDown path; a focused input (rename,
  // address) captures its own keys before this runs.
  const onKeyDown = useCallback((rawEvent: unknown) => {
    const key = rawEvent as unknown as LiteKeyEvent;
    if (key.value === 0) return;
    if (dialog) {
      if (key.code === KEY_ESC) closeDialog();
      return;
    }
    const control = (key.modifiers & MOD_CONTROL) !== 0;
    if (control && key.code === KEY_A) browser.selectAll();
    else if (control && key.code === KEY_X) browser.clipboardFromSelection("cut", selectedEntries);
    else if (control && key.code === KEY_C) browser.clipboardFromSelection("copy", selectedEntries);
    else if (control && key.code === KEY_V) browser.paste();
    else if (key.code === KEY_BACKSPACE) { if (!atRoot) browser.up(); }
    else if (key.code === KEY_ENTER && focusedEntry) browser.openEntry(focusedEntry);
    else if (key.code === KEY_F2 && focusedEntry) browser.beginRename(focusedEntry.name);
    else if (key.code === KEY_DELETE && selectedEntries.length > 0) browser.deleteSelected(selectedEntries);
  }, [dialog, closeDialog, browser, selectedEntries, focusedEntry, atRoot]);

  const visibleEntries = useMemo(() => {
    const filtered = entries.filter((entry) =>
      entry.name.toLowerCase().includes(query.trim().toLowerCase()),
    );
    if (!atRoot) return filtered;
    const order = ["Documents", "Downloads", "Pictures", "Music", "Videos"];
    return filtered.sort((left, right) => {
      const leftIndex = order.indexOf(left.name);
      const rightIndex = order.indexOf(right.name);
      return (leftIndex < 0 ? order.length : leftIndex)
        - (rightIndex < 0 ? order.length : rightIndex);
    });
  }, [entries, query, atRoot]);

  // Home splits its content into "Folders" (directory grid) and "Recent files"
  // (real files under the home tree, newest first). Only computed at root, and
  // filtered by the same search query as the grid.
  const homeFolders = useMemo(
    () => visibleEntries.filter((entry) => entry.kind === "dir" || entry.kind === "symlink"),
    [visibleEntries],
  );
  const recentFiles = useMemo(() => {
    if (!atRoot) return [];
    const normalized = query.trim().toLowerCase();
    return collectRecentFiles("/root", 6)
      .filter((file) => file.name.toLowerCase().includes(normalized));
    // Re-scan when the query changes or we (re)enter Home via a browser refresh.
  }, [atRoot, query, browser.entries]);
  // Refresh capacity whenever the browser publishes a new listing. A cached
  // fixed fill would drift after mutations and falsely present itself as live.
  const storage = useMemo(() => capacity("/"), [browser.entries]);
  const storageTotal = storage.totalBytes ?? 0;
  const storageUsed = storage.usedBytes ?? 0;
  const storagePercent = storageTotal === 0
    ? 0
    : Math.min(100, Math.max(0, storageUsed / storageTotal * 100));
  const storageLabel = storage.error || storageTotal === 0
    ? "Capacity unavailable"
    : `${formatCapacity(storageUsed)} / ${formatCapacity(storageTotal)}`;

  return (
    <div className="aurora-root explorer explorer--files" onClick={closeMenu} onKeyDown={onKeyDown}>
      <div className="explorer__body">
        <Sidebar className="files-sidebar">
          <SidebarItem label="Home" icon="assets/sidebar-home.png" active={path === "/root"} onClick={() => browser.navigate("/root")}/>
          <SidebarItem label="Documents" icon="assets/sidebar-documents.png" active={path === "/root/Documents"} onClick={() => browser.navigate("/root/Documents")}/>
          <SidebarItem label="Downloads" icon="assets/sidebar-downloads.png" active={path === "/root/Downloads"} onClick={() => browser.navigate("/root/Downloads")}/>
          <SidebarItem label="Pictures" icon="assets/sidebar-pictures.png" active={path === "/root/Pictures"} onClick={() => browser.navigate("/root/Pictures")}/>
          <SidebarItem label="Music" icon="assets/sidebar-music.png" active={path === "/root/Music"} onClick={() => browser.navigate("/root/Music")}/>
          <SidebarItem label="Videos" icon="assets/sidebar-videos.png" active={path === "/root/Videos"} onClick={() => browser.navigate("/root/Videos")}/>
          <div className="sidebar-separator"/>
          <SidebarItem label="Storage" icon="assets/sidebar-storage.png" active={path === "/"} onClick={() => browser.navigate("/")}/>
          <div className="files-storage">
            <img className="files-storage__icon" src="assets/sidebar-storage.png"/>
            <div className="files-storage__details">
              <span className="files-storage__label">{storageLabel}</span>
              <div className="files-storage__track">
                <div className="files-storage__fill" style={{ width: `${storagePercent}%` }}/>
              </div>
            </div>
          </div>
        </Sidebar>
        <div className="files-content">
          <div className="files-toolbar">
            <div className="files-nav">
              <button className="files-nav__btn" aria-label="Back" disabled={!browser.canBack} onClick={browser.back}>
                <img src="assets/nav-back.png"/>
              </button>
              <button className="files-nav__btn" aria-label="Forward" disabled={!browser.canForward} onClick={browser.forward}>
                <img src="assets/nav-forward.png"/>
              </button>
              <button className="files-nav__btn" aria-label="Up" disabled={!browser.canUp} onClick={browser.up}>
                <img src="assets/nav-up.png"/>
              </button>
              <button className="files-nav__btn" aria-label="Home" disabled={atRoot} onClick={() => browser.navigate("/root")}>
                <img src="assets/nav-home.png"/>
              </button>
            </div>
            <ViewSwitch
              mode={viewMode === "icons" ? "icons" : "details"}
              onChange={setViewMode}
            />
            <SearchField value={query} placeholder="Search files" onInput={setQuery}/>
          </div>
          {atRoot && viewMode === "icons" ? (
            <div className="files-home">
              {error && <div className="folder-view__note">{error}</div>}
              <div className="files-home__section">
                <span className="cat-heading">Folders</span>
                <div className="icon-grid">
                  {homeFolders.map((entry) => (
                    <button
                      key={entry.name}
                      className={`icon-cell${selected.includes(entry.name) ? " icon-cell--sel" : ""}`}
                      onClick={() => browser.selectOnly(entry.name)}
                      onDoubleClick={() => browser.openEntry(entry)}
                      onContextMenu={(rawEvent) => {
                        const event = rawEvent as unknown as { x: number; y: number };
                        browser.selectOnly(entry.name);
                        openMenu(event.x, event.y, rowMenu(entry));
                      }}
                    >
                      <img className="icon-cell__img" src={iconFor(entry)}/>
                      <span className="icon-cell__label control-label">{entry.name}</span>
                    </button>
                  ))}
                  {homeFolders.length === 0 && <span className="command-empty">No folders</span>}
                </div>
              </div>
              <div className="files-home__section">
                <span className="cat-heading">Recent files</span>
                <div className="recent-list">
                  {recentFiles.length === 0 ? (
                    <span className="command-empty">No recent files</span>
                  ) : (
                    recentFiles.map((file) => (
                      <button
                        key={file.path}
                        className="recent-row"
                        onDoubleClick={() => browser.navigate(parentPath(file.path))}
                      >
                        <img className="recent-row__icon" src="assets/file-16.png"/>
                        <span className="recent-row__name control-label">{file.name}</span>
                        <span className="recent-row__when control-label">{formatRecent(file.mtime, now)}</span>
                      </button>
                    ))
                  )}
                </div>
              </div>
            </div>
          ) : (
            <FolderView
              viewMode={viewMode} entries={visibleEntries} error={error}
              iconLarge={iconFor} iconSmall={iconFor16}
              entryType={(entry) => typeLabel(entry, TYPE_LABELS)}
              columns={{ name: "Name", size: "Size", type: "Type", mtime: "Date Modified" }}
              formatDate={formatDateEn}
              sort={browser.sort} onSort={(column: SortColumn) => browser.toggleSort(column)}
              selected={selected} renaming={browser.renaming} renameDraft={browser.renameDraft} renameMaxWidth={90}
              onSelect={(entry) => browser.selectOnly(entry.name)}
              onOpen={browser.openEntry}
              onEntryMenu={(entry, x, y) => { browser.selectOnly(entry.name); openMenu(x, y, rowMenu(entry)); }}
              onBlankMenu={(x, y) => openMenu(x, y, emptyMenu())}
              onRenameDraftChange={browser.setRenameDraft}
              onRenameCommit={browser.commitRename}
              onRenameCancel={browser.cancelRename}
            />
          )}
        </div>
      </div>

      {menu && <ContextMenu x={menu.x} y={menu.y} items={menu.items} onClose={closeMenu}/>}
      {dialog?.kind === "viewer" && (
        <TextViewer view={dialog.view} onClose={closeDialog} closeLabel="Close" truncatedLabel="(content truncated at 64 KB)"/>
      )}
      {dialog?.kind === "cannot-open" && (
        <CannotOpenDialog name={dialog.name} message={(name) => `LiteOS cannot open '${name}'. No program is associated with this file type.`} onClose={closeDialog} closeLabel="OK"/>
      )}
    </div>
  );
}
