import React, { useCallback, useMemo, useState } from "react";
import type { FsEntry } from "lite:fs";
import { ContextMenu } from "../design-system/context-menu.tsx";
import {
  AddressBar,
  GroupBox,
  MenuBar,
  StatusBar,
  StatusBarCell,
  TaskLink,
  Toolbar,
  ToolbarButton,
  ToolbarSeparator,
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
import type { SortColumn, ViewMode } from "../explorer/use-browser.ts";
import {
  FolderTree,
  FolderView,
  blankMenu,
  entryMenu,
  subdirs,
} from "../explorer/components.tsx";
import {
  CannotOpenDialog,
  FolderOptionsDialog,
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

/** The one open modal, or null. Viewer/cannot-open come from opening a file;
 * options is Tools → Folder Options. */
type DialogState =
  | { kind: "viewer"; view: FileView }
  | { kind: "cannot-open"; name: string }
  | { kind: "options" }
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

// Menubar geometry: English labels take the narrow stride.
const MENU_LABEL_X = 8;
const MENU_LABEL_STRIDE = 42;
// Back/Forward history dropdowns open under their toolbar buttons (the same
// hardcoded-geometry pattern as the menubar).
const BACK_MENU_X = 8;
const FORWARD_MENU_X = 80;
const NAV_MENU_Y = 64;

export default function FileManager() {
  const [dialog, setDialog] = useState<DialogState>(null);
  const closeDialog = useCallback(() => setDialog(null), []);

  const openFile = useCallback((path: string, entry: FsEntry) => {
    const result = readTextFile(path, entry);
    if ("view" in result) setDialog({ kind: "viewer", view: result.view });
    else setDialog({ kind: "cannot-open", name: entry.name });
  }, []);

  const browser = useBrowser("/", { typeLabels: TYPE_LABELS, onOpenFile: openFile });
  const { path, entries, error, viewMode, setViewMode, selected } = browser;
  const [expanded, setExpanded] = useState<Record<string, boolean>>({
    tasks: true,
    places: true,
  });
  const [foldersPane, setFoldersPane] = useState(false);
  const [menu, setMenu] = useState<MenuState | null>(null);

  const closeMenu = useCallback(() => setMenu(null), []);
  const openMenu = useCallback((x: number, y: number, items: MenuItem[]) => {
    setMenu({ x, y, items });
  }, []);

  const atRoot = path === "/";
  const selectedEntries = useMemo(
    () => entries.filter((entry) => selected.includes(entry.name)),
    [entries, selected],
  );
  const focusedEntry = selectedEntries.at(-1) ?? null;
  const newFolder = useCallback(() => browser.newFolder("New Folder"), [browser]);
  const toggleGroup = (id: string) =>
    setExpanded((current) => ({ ...current, [id]: !current[id] }));

  // Back/Forward history dropdowns (XP's chevrons beside the buttons).
  const historyMenu = useCallback((direction: "back" | "forward"): MenuItem[] => {
    const { history, historyIndex } = browser;
    const range = direction === "back"
      ? history.slice(0, historyIndex).map((entry, index) => ({ entry, index })).reverse()
      : history.slice(historyIndex + 1).map((entry, offset) => ({ entry, index: historyIndex + 1 + offset }));
    return range.map(({ entry, index }) => ({
      id: String(index),
      label: entry === "/" ? "My Computer" : entry,
      onSelect: () => browser.jumpTo(index),
    }));
  }, [browser]);

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

  // Menubar dropdowns. Dead chrome (Favorites/Help without real targets) was
  // removed; Tools carries the working Folder Options dialog.
  const menus: { label: string; items: MenuItem[] | null }[] = [
    {
      label: "File",
      items: [
        { id: "new", label: "New Folder", onSelect: newFolder },
        { id: "refresh", label: "Refresh", onSelect: browser.refresh },
      ],
    },
    {
      label: "Edit",
      items: [
        { id: "cut", label: "Cut", onSelect: () => browser.clipboardFromSelection("cut", selectedEntries) },
        { id: "copy", label: "Copy", onSelect: () => browser.clipboardFromSelection("copy", selectedEntries) },
        { id: "paste", label: "Paste", onSelect: browser.paste },
      ],
    },
    {
      label: "View",
      items: [
        { id: "icons", label: "Icons", onSelect: () => setViewMode("icons") },
        { id: "list", label: "List", onSelect: () => setViewMode("list") },
        { id: "details", label: "Details", onSelect: () => setViewMode("details") },
        { id: "refresh", label: "Refresh", onSelect: browser.refresh },
      ],
    },
    {
      label: "Tools",
      items: [
        { id: "options", label: "Folder Options", onSelect: () => setDialog({ kind: "options" }) },
      ],
    },
  ];

  // Address caret: a dropdown of ancestor directories, each navigable.
  const ancestors = useMemo(() => {
    const parts = path.split("/").filter(Boolean);
    const list: MenuItem[] = [{ id: "/", label: "My Computer", onSelect: () => browser.navigate("/") }];
    let acc = "";
    for (const part of parts) {
      acc = `${acc}/${part}`;
      const full = acc;
      list.push({ id: full, label: full, onSelect: () => browser.navigate(full) });
    }
    return list;
  }, [path, browser]);

  // XP keyboard map on the global onKeyDown path; a focused input (rename,
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

  const statusText = selected.length > 0
    ? `${selected.length} object(s) selected`
    : `${entries.length} objects`;

  return (
    <div className="explorer" onClick={closeMenu} onKeyDown={onKeyDown}>
      <MenuBar menus={menus} labelX={MENU_LABEL_X} stride={MENU_LABEL_STRIDE}/>

      <Toolbar>
        <ToolbarButton icon="assets/tb-back.png" label="Back" disabled={!browser.canBack} dropdown={{ items: historyMenu("back"), at: { x: BACK_MENU_X, y: NAV_MENU_Y } }} onClick={browser.back}/>
        <ToolbarButton icon="assets/tb-forward.png" label="Forward" disabled={!browser.canForward} dropdown={{ items: historyMenu("forward"), at: { x: FORWARD_MENU_X, y: NAV_MENU_Y } }} onClick={browser.forward}/>
        <ToolbarButton icon="assets/tb-up.png" disabled={atRoot} onClick={browser.up}/>
        <ToolbarSeparator/>
        <ToolbarButton icon="assets/tb-folders.png" label="New Folder" onClick={newFolder}/>
        <ToolbarSeparator/>
        <ToolbarButton icon="assets/tb-folders.png" label="Folders" onClick={() => setFoldersPane((value) => !value)}/>
        <ToolbarButton icon="assets/tb-views.png" label="Views" onClick={() => setViewMode((mode: ViewMode) => (mode === "icons" ? "details" : "icons"))}/>
      </Toolbar>

      <AddressBar
        label="Address" icon="assets/folder-16.png" text={path}
        draft={browser.addressDraft}
        onBeginEdit={() => browser.setAddressDraft(path)}
        onDraftChange={browser.setAddressDraft}
        onCommit={() => browser.navigate(browser.addressDraft ?? path)}
        onCancel={() => browser.setAddressDraft(null)}
        dropItems={ancestors}
        go={{ label: "Go", icon: "assets/tb-forward.png", onClick: browser.refresh }}
      />

      <div className="explorer__body">
        {foldersPane ? (
          <FolderTree
            roots={[{ path: "/", label: "My Computer", icon: "assets/computer.png" }]}
            currentPath={path}
            listDirs={subdirs}
            onNavigate={browser.navigate}
          />
        ) : (
          <div className="task-pane">
            <GroupBox title="File and Folder Tasks" expanded={expanded.tasks} onToggle={() => toggleGroup("tasks")}>
              <TaskLink label="Make a new folder" onClick={newFolder}/>
              {focusedEntry && <TaskLink label="Rename this item" onClick={() => browser.beginRename(focusedEntry.name)}/>}
              {selectedEntries.length > 0 && <TaskLink label="Delete this item" onClick={() => browser.deleteSelected(selectedEntries)}/>}
            </GroupBox>
            <GroupBox title="Other Places" expanded={expanded.places} onToggle={() => toggleGroup("places")}>
              {!atRoot && (
                <TaskLink label={parentPath(path)} onClick={browser.up}/>
              )}
              <TaskLink label="My Computer" onClick={() => browser.navigate("/")}/>
            </GroupBox>
          </div>
        )}

        <FolderView
          viewMode={viewMode} entries={entries} error={error}
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

      <StatusBar>
        <StatusBarCell text={statusText}/>
      </StatusBar>

      {menu && <ContextMenu x={menu.x} y={menu.y} items={menu.items} onClose={closeMenu}/>}
      {dialog?.kind === "viewer" && (
        <TextViewer view={dialog.view} onClose={closeDialog} closeLabel="Close" truncatedLabel="(content truncated at 64 KB)"/>
      )}
      {dialog?.kind === "cannot-open" && (
        <CannotOpenDialog name={dialog.name} message={(name) => `Windows cannot open '${name}'. No program is associated with this file type.`} onClose={closeDialog} closeLabel="OK"/>
      )}
      {dialog?.kind === "options" && (
        <FolderOptionsDialog
          showHidden={browser.showHidden}
          onToggleHidden={() => browser.setShowHidden(!browser.showHidden)}
          viewMode={viewMode}
          views={[{ mode: "icons", label: "Icons" }, { mode: "list", label: "List" }, { mode: "details", label: "Details" }]}
          onPickView={setViewMode}
          onClose={closeDialog}
          labels={{ hiddenFiles: "Show hidden files and folders", view: "Folder Options", close: "OK" }}
        />
      )}
    </div>
  );
}
