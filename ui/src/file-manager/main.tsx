import React, { useCallback, useMemo, useState } from "react";
import { capacity, list } from "lite:fs";
import type { FsEntry } from "lite:fs";
import { ContextMenu } from "../design-system/context-menu.tsx";
import {
  AddressBar,
  SearchField,
  Sidebar,
  SidebarItem,
  StatusBar,
  StatusBarCell,
  ViewSwitch,
} from "../design-system/controls.tsx";
import type { MenuItem } from "../design-system/controls.tsx";
import {
  KEY_A, KEY_BACKSPACE, KEY_C, KEY_DELETE, KEY_DOWN, KEY_ESC, KEY_ENTER, KEY_F2, KEY_F5,
  KEY_L, KEY_LEFT, KEY_N, KEY_RIGHT, KEY_UP, KEY_V, KEY_X,
  MOD_ALT, MOD_CONTROL, MOD_SHIFT,
  formatDateEn,
  formatFsError,
  formatRecent,
  joinPath,
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
  DeleteConfirmDialog,
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
  | { kind: "delete"; entries: FsEntry[] }
  | null;

const TYPE_LABELS: TypeLabels = {
  folder: "File Folder",
  shortcut: "Shortcut",
  file: "File",
  extensionFile: (extension) => `${extension} File`,
};

const describeError = (code: string) => formatFsError(code, "en");

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
  entry: FsEntry;
  directory: string;
  path: string;
}

const HOME_PLACES = [
  "/root/Documents",
  "/root/Downloads",
  "/root/Pictures",
  "/root/Music",
  "/root/Videos",
] as const;

/** Maps any current path to the most specific persistent sidebar place. */
function sidebarPlace(path: string): string {
  const known = HOME_PLACES.find((place) => path === place || path.startsWith(`${place}/`));
  if (known) return known;
  if (path === "/root" || path.startsWith("/root/")) return "/root";
  return "/";
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
        files.push({ entry, directory: dir, path: joinPath(dir, entry.name) });
      }
    }
  };
  scan(root);
  for (const entry of list(root).entries ?? []) {
    if (entry.kind === "dir" && !entry.name.startsWith(".")) scan(joinPath(root, entry.name));
  }
  return files.sort((a, b) => b.entry.mtime - a.entry.mtime).slice(0, limit);
}

export default function FileManager() {
  const [dialog, setDialog] = useState<DialogState>(null);
  const closeDialog = useCallback(() => setDialog(null), []);

  const openFile = useCallback((path: string, entry: FsEntry) => {
    const result = readTextFile(path, entry);
    if ("view" in result) setDialog({ kind: "viewer", view: result.view });
    else setDialog({ kind: "cannot-open", name: entry.name });
  }, []);

  const browser = useBrowser("/root", { typeLabels: TYPE_LABELS, onOpenFile: openFile, describeError });
  const { path, entries, error, notice, viewMode, setViewMode, selected } = browser;
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [query, setQuery] = useState("");
  // One clock read for the Home "Recent files" relative stamps. Snapshotting
  // once (not per render) keeps the list stable and avoids an argless
  // new Date() during render; refreshing on navigation into Home is enough.
  const [now] = useState(() => new Date());

  const closeMenu = useCallback(() => setMenu(null), []);
  const openMenu = useCallback((x: number, y: number, items: MenuItem[]) => {
    browser.cancelRename();
    browser.setAddressDraft(null);
    setMenu({ x, y, items });
  }, [browser]);

  const atRoot = path === "/root";
  const selectedEntries = useMemo(
    () => entries.filter((entry) => selected.includes(entry.name)),
    [entries, selected],
  );
  const focusedEntry = selectedEntries.at(-1) ?? null;
  const newFolder = useCallback(() => browser.newFolder("New Folder"), [browser]);
  const requestDelete = useCallback((targets: FsEntry[]) => {
    if (targets.length > 0) setDialog({ kind: "delete", entries: targets });
  }, []);
  const confirmDelete = useCallback(() => {
    if (dialog?.kind !== "delete") return;
    const targets = dialog.entries;
    setDialog(null);
    browser.deleteSelected(targets);
  }, [browser, dialog]);

  // Row context menu: Open/Cut/Copy/Delete/Rename operate on the selection
  // the clicked row belongs to.
  const rowMenu = useCallback((entry: FsEntry): MenuItem[] => entryMenu(
    { open: "Open", cut: "Cut", copy: "Copy", delete: "Delete", rename: "Rename" },
    {
      // A context click inside the current selection acts on that selection;
      // otherwise it acts only on the clicked entry. Capturing the old
      // selection here would make destructive verbs target invisible rows.
      onOpen: () => browser.openEntry(entry),
      onCut: () => browser.clipboardFromSelection("cut", selected.includes(entry.name) ? selectedEntries : [entry]),
      onCopy: () => browser.clipboardFromSelection("copy", selected.includes(entry.name) ? selectedEntries : [entry]),
      onDelete: () => requestDelete(selected.includes(entry.name) ? selectedEntries : [entry]),
      onRename: () => browser.beginRename(entry.name),
    },
  ), [browser, selected, selectedEntries, requestDelete]);

  // Empty-area context menu: New Folder, Paste (only with a clipboard), Refresh.
  const emptyMenu = useCallback((): MenuItem[] => blankMenu(
    { newFolder: "New Folder", paste: "Paste", refresh: "Refresh" },
    {
      onNewFolder: newFolder,
      onPaste: browser.clipboard ? browser.paste : undefined,
      onRefresh: browser.refresh,
    },
  ), [newFolder, browser]);

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
  const keyboardEntries = atRoot && viewMode === "icons"
    ? visibleEntries.filter((entry) => entry.kind === "dir" || entry.kind === "symlink")
    : visibleEntries;

  // Explorer keyboard map on the global onKeyDown path; a focused input (rename,
  // address) captures its own keys before this runs.
  const onKeyDown = useCallback((rawEvent: unknown) => {
    const key = rawEvent as unknown as LiteKeyEvent;
    if (key.value === 0) return;
    if (dialog) {
      if (key.code === KEY_ESC) closeDialog();
      return;
    }
    if (menu) {
      if (key.code === KEY_ESC) closeMenu();
      return;
    }
    const initial = key.value === 1;
    const control = (key.modifiers & MOD_CONTROL) !== 0;
    const alt = (key.modifiers & MOD_ALT) !== 0;
    const shift = (key.modifiers & MOD_SHIFT) !== 0;
    if (initial && control && key.code === KEY_A) browser.selectAll(keyboardEntries);
    else if (initial && control && shift && key.code === KEY_N) newFolder();
    else if (initial && control && key.code === KEY_L) browser.setAddressDraft(path);
    else if (initial && control && key.code === KEY_X) browser.clipboardFromSelection("cut", selectedEntries);
    else if (initial && control && key.code === KEY_C) browser.clipboardFromSelection("copy", selectedEntries);
    else if (initial && control && key.code === KEY_V) browser.paste();
    else if (initial && alt && key.code === KEY_LEFT) browser.back();
    else if (initial && alt && key.code === KEY_RIGHT) browser.forward();
    else if (initial && key.code === KEY_BACKSPACE) { if (!atRoot) browser.up(); }
    else if (initial && key.code === KEY_ENTER && focusedEntry) browser.openEntry(focusedEntry);
    else if (initial && key.code === KEY_F2 && focusedEntry) browser.beginRename(focusedEntry.name);
    else if (initial && key.code === KEY_DELETE && selectedEntries.length > 0) requestDelete(selectedEntries);
    else if (initial && key.code === KEY_F5) browser.refresh();
    else if (initial && key.code === KEY_ESC) {
      if (query) {
        setQuery("");
        browser.clearSelection();
      } else if (browser.clipboard?.mode === "cut") browser.setClipboard(null);
      else browser.clearSelection();
    }
    else if (!control && !alt && (key.code === KEY_UP || key.code === KEY_LEFT)) browser.selectRelative(keyboardEntries, -1);
    else if (!control && !alt && (key.code === KEY_DOWN || key.code === KEY_RIGHT)) browser.selectRelative(keyboardEntries, 1);
  }, [dialog, closeDialog, menu, closeMenu, browser, selectedEntries, focusedEntry, path, query, atRoot, requestDelete, keyboardEntries, newFolder]);

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
      .filter((file) => file.entry.name.toLowerCase().includes(normalized));
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
  const activePlace = sidebarPlace(path);
  const statusText = selected.length > 0
    ? `${selected.length} item${selected.length === 1 ? "" : "s"} selected`
    : `${visibleEntries.length} item${visibleEntries.length === 1 ? "" : "s"}`;
  const clipboardText = browser.clipboard
    ? `${browser.clipboard.mode === "cut" ? "Cut" : "Copied"} ${browser.clipboard.paths.length} item${browser.clipboard.paths.length === 1 ? "" : "s"}${browser.clipboard.mode === "cut" ? " (Esc cancels)" : ""} — choose a folder and Paste`
    : null;

  return (
    <div className="aurora-root explorer explorer--files" onClick={closeMenu} onKeyDown={onKeyDown}>
      <div className="explorer__body">
        <Sidebar className="files-sidebar">
          <SidebarItem label="Home" icon="assets/sidebar-home.png" active={activePlace === "/root"} onClick={() => browser.navigate("/root")}/>
          <SidebarItem label="Documents" icon="assets/sidebar-documents.png" active={activePlace === "/root/Documents"} onClick={() => browser.navigate("/root/Documents")}/>
          <SidebarItem label="Downloads" icon="assets/sidebar-downloads.png" active={activePlace === "/root/Downloads"} onClick={() => browser.navigate("/root/Downloads")}/>
          <SidebarItem label="Pictures" icon="assets/sidebar-pictures.png" active={activePlace === "/root/Pictures"} onClick={() => browser.navigate("/root/Pictures")}/>
          <SidebarItem label="Music" icon="assets/sidebar-music.png" active={activePlace === "/root/Music"} onClick={() => browser.navigate("/root/Music")}/>
          <SidebarItem label="Videos" icon="assets/sidebar-videos.png" active={activePlace === "/root/Videos"} onClick={() => browser.navigate("/root/Videos")}/>
          <div className="sidebar-separator"/>
          <SidebarItem label="Storage" icon="assets/sidebar-storage.png" active={activePlace === "/"} onClick={() => browser.navigate("/")}/>
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
            <SearchField
              value={query}
              placeholder="Search files"
              onInput={(value) => {
                setQuery(value);
                browser.clearSelection();
              }}
              onEscape={() => {
                setQuery("");
                browser.clearSelection();
              }}
            />
          </div>
          <AddressBar
            label="Location"
            icon={path === "/" ? "assets/sidebar-storage.png" : "assets/folder-16.png"}
            text={path === "/root" ? "Home — /root" : path}
            draft={browser.addressDraft}
            onBeginEdit={() => browser.setAddressDraft(path)}
            onDraftChange={browser.setAddressDraft}
            onCommit={() => browser.navigate(browser.addressDraft ?? path)}
            onCancel={() => browser.setAddressDraft(null)}
          />
          {atRoot && viewMode === "icons" ? (
            <div
              className="files-home"
              onClick={() => browser.clearSelection()}
              onContextMenu={(rawEvent) => {
                const event = rawEvent as unknown as LitePointerEvent;
                browser.clearSelection();
                openMenu(event.x, event.y, emptyMenu());
              }}
            >
              {error && <div className="folder-view__note">{error}</div>}
              {notice && <div className="folder-view__notice">{notice}</div>}
              <div className="files-home__section">
                <span className="cat-heading">Folders</span>
                <div className="icon-grid">
                  {homeFolders.map((entry) => (
                    <button
                      key={entry.name}
                      className={`icon-cell${selected.includes(entry.name) ? " icon-cell--sel" : ""}${browser.cutNames.includes(entry.name) ? " icon-cell--cut" : ""}`}
                      onClick={(rawEvent) => {
                        const event = rawEvent as unknown as LitePointerEvent;
                        event.stopPropagation();
                        if (event.keyboard) browser.openEntry(entry);
                        else browser.selectWithModifiers(homeFolders, entry.name, event.modifiers);
                      }}
                      onDoubleClick={() => browser.openEntry(entry)}
                      onContextMenu={(rawEvent) => {
                        const event = rawEvent as unknown as LitePointerEvent;
                        event.stopPropagation();
                        if (!selected.includes(entry.name)) browser.selectOnly(entry.name);
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
                        onClick={() => openFile(file.directory, file.entry)}
                        onContextMenu={(rawEvent) => {
                          const event = rawEvent as unknown as LitePointerEvent;
                          event.stopPropagation();
                          openMenu(event.x, event.y, [
                            { id: "open", label: "Open", onSelect: () => openFile(file.directory, file.entry) },
                            { id: "location", label: "Open Containing Folder", onSelect: () => browser.navigate(file.directory) },
                          ]);
                        }}
                      >
                        <img className="recent-row__icon" src="assets/file-16.png"/>
                        <span className="recent-row__name control-label">{file.entry.name}</span>
                        <span className="recent-row__when control-label">{formatRecent(file.entry.mtime, now)}</span>
                      </button>
                    ))
                  )}
                </div>
              </div>
            </div>
          ) : (
            <FolderView
              viewMode={viewMode} entries={visibleEntries} error={error} notice={notice}
              emptyLabel={query.trim()
                ? "No files match your search"
                : "This folder is empty — press Ctrl+Shift+N to create a folder"}
              iconLarge={iconFor} iconSmall={iconFor16}
              entryType={(entry) => typeLabel(entry, TYPE_LABELS)}
              columns={{ name: "Name", size: "Size", type: "Type", mtime: "Date Modified" }}
              formatDate={formatDateEn}
              sort={browser.sort} onSort={(column: SortColumn) => browser.toggleSort(column)}
              selected={selected} cut={browser.cutNames} renaming={browser.renaming} renameDraft={browser.renameDraft} renameMaxWidth={90}
              onSelect={(entry, modifiers) => browser.selectWithModifiers(visibleEntries, entry.name, modifiers)}
              onOpen={browser.openEntry}
              onEntryMenu={(entry, x, y) => {
                if (!selected.includes(entry.name)) browser.selectOnly(entry.name);
                openMenu(x, y, rowMenu(entry));
              }}
              onBlankMenu={(x, y) => {
                browser.clearSelection();
                openMenu(x, y, emptyMenu());
              }}
              onBlankClick={() => browser.clearSelection()}
              onRenameDraftChange={browser.setRenameDraft}
              onRenameCommit={browser.commitRename}
              onRenameCancel={browser.cancelRename}
            />
          )}
        </div>
      </div>

      <StatusBar>
        <StatusBarCell text={statusText}/>
        <StatusBarCell icon={path === "/" ? "assets/sidebar-storage.png" : "assets/folder-16.png"} text={path === "/root" ? "Home" : path}/>
        {clipboardText && <StatusBarCell text={clipboardText}/>}
      </StatusBar>

      {menu && <ContextMenu x={menu.x} y={menu.y} items={menu.items} onClose={closeMenu}/>}
      {dialog?.kind === "viewer" && (
        <TextViewer view={dialog.view} onClose={closeDialog} closeLabel="Close" truncatedLabel="(content truncated at 64 KB)"/>
      )}
      {dialog?.kind === "cannot-open" && (
        <CannotOpenDialog name={dialog.name} message={(name) => `LiteOS cannot open '${name}'. No program is associated with this file type.`} onClose={closeDialog} closeLabel="OK"/>
      )}
      {dialog?.kind === "delete" && (
        <DeleteConfirmDialog
          title="Delete permanently?"
          message={dialog.entries.length === 1
            ? `Delete '${dialog.entries[0].name}'? This action cannot be undone.`
            : `Delete ${dialog.entries.length} selected items? This action cannot be undone.`}
          deleteLabel="Delete"
          cancelLabel="Cancel"
          onConfirm={confirmDelete}
          onClose={closeDialog}
        />
      )}
    </div>
  );
}
