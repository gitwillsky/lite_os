import React, { useCallback, useMemo, useState } from "react";
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
    ? "assets/files.png"
    : "assets/file.png";
}

/** 16px icon for the details view and address bar. */
function iconFor16(entry: FsEntry): string {
  return entry.kind === "dir" || entry.kind === "symlink"
    ? "assets/folder-16.png"
    : "assets/file-16.png";
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

  return (
    <div className="aurora-root explorer explorer--files" onClick={closeMenu} onKeyDown={onKeyDown}>
      <div className="explorer__body">
        <Sidebar className="files-sidebar">
          <SidebarItem label="Home" glyph="home" active={path === "/root"} onClick={() => browser.navigate("/root")}/>
          <SidebarItem label="Documents" glyph="document" active={path === "/root/Documents"} onClick={() => browser.navigate("/root/Documents")}/>
          <SidebarItem label="Downloads" glyph="download" active={path === "/root/Downloads"} onClick={() => browser.navigate("/root/Downloads")}/>
          <SidebarItem label="Pictures" glyph="picture" active={path === "/root/Pictures"} onClick={() => browser.navigate("/root/Pictures")}/>
          <SidebarItem label="Music" glyph="music" active={path === "/root/Music"} onClick={() => browser.navigate("/root/Music")}/>
          <SidebarItem label="Videos" glyph="video" active={path === "/root/Videos"} onClick={() => browser.navigate("/root/Videos")}/>
          <div className="sidebar-separator"/>
          <SidebarItem label="Storage" glyph="storage" active={path === "/"} onClick={() => browser.navigate("/")}/>
        </Sidebar>
        <div className="files-content">
          <div className="files-toolbar">
            <ViewSwitch
              mode={viewMode === "icons" ? "icons" : "details"}
              onChange={setViewMode}
            />
            <SearchField value={query} placeholder="Search files" onInput={setQuery}/>
          </div>
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
        </div>
      </div>

      {menu && <ContextMenu x={menu.x} y={menu.y} items={menu.items} onClose={closeMenu}/>}
      {dialog?.kind === "viewer" && (
        <TextViewer view={dialog.view} onClose={closeDialog} closeLabel="Close" truncatedLabel="(content truncated at 64 KB)"/>
      )}
      {dialog?.kind === "cannot-open" && (
        <CannotOpenDialog name={dialog.name} message={(name) => `Windows cannot open '${name}'. No program is associated with this file type.`} onClose={closeDialog} closeLabel="OK"/>
      )}
    </div>
  );
}
