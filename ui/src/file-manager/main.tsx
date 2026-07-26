import React, { useCallback, useMemo, useState } from "react";
import type { FsEntry } from "lite:fs";
import { ContextMenu } from "../design-system/context-menu.tsx";
import { joinPath, parentPath, typeLabel } from "../explorer/model.ts";
import type { TypeLabels } from "../explorer/model.ts";
import { useBrowser } from "../explorer/use-browser.ts";
import type { ViewMode } from "../explorer/use-browser.ts";
import {
  AddressBar,
  FolderView,
  GroupBox,
  MenuBar,
  TbButton,
  TbSeparator,
  TaskLink,
  blankMenu,
  entryMenu,
} from "../explorer/components.tsx";
import type { MenuItem } from "../explorer/components.tsx";

/** An open context menu: viewport-local position and its rows. */
interface MenuState {
  x: number;
  y: number;
  items: MenuItem[];
}

const TYPE_LABELS: TypeLabels = {
  folder: "File Folder",
  shortcut: "Shortcut",
  file: "File",
  extensionFile: (extension) => `${extension} File`,
};

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

// Fixed menubar geometry: dropdowns open just under the clicked label. The
// menubar is a fixed row, so a per-label x offset and a constant y suffice.
const MENUBAR_TOP = 40;
const MENU_LABEL_X = 8;
const MENU_LABEL_STRIDE = 42;

export default function FileManager() {
  const browser = useBrowser("/");
  const { path, entries, error, viewMode, setViewMode, selected, setSelected } = browser;
  const { hover } = browser;
  const [expanded, setExpanded] = useState<Record<string, boolean>>({
    tasks: true,
    places: true,
  });
  const [menu, setMenu] = useState<MenuState | null>(null);

  const closeMenu = useCallback(() => setMenu(null), []);
  const openMenu = useCallback((x: number, y: number, items: MenuItem[]) => {
    setMenu({ x, y, items });
  }, []);

  const atRoot = path === "/";
  const selectedEntry = entries.find((entry) => entry.name === selected) ?? null;
  const newFolder = useCallback(() => browser.newFolder("New Folder"), [browser]);
  const toggleGroup = (id: string) =>
    setExpanded((current) => ({ ...current, [id]: !current[id] }));

  // Row context menu: Open/Cut/Copy/Delete/Rename operate on one entry.
  const rowMenu = useCallback((entry: FsEntry): MenuItem[] => entryMenu(
    { open: "Open", cut: "Cut", copy: "Copy", delete: "Delete", rename: "Rename" },
    {
      onOpen: () => browser.openEntry(entry),
      onCut: () => browser.setClipboard({ mode: "cut", path: joinPath(path, entry.name) }),
      onCopy: () => browser.setClipboard({ mode: "copy", path: joinPath(path, entry.name) }),
      onDelete: () => browser.deleteEntry(entry),
      onRename: () => browser.beginRename(entry.name),
    },
  ), [browser, path]);

  // Empty-area context menu: New Folder, Paste (only with a clipboard), Refresh.
  const emptyMenu = useCallback((): MenuItem[] => blankMenu(
    { newFolder: "New Folder", paste: "Paste", refresh: "Refresh" },
    {
      onNewFolder: newFolder,
      onPaste: browser.clipboard ? browser.paste : undefined,
      onRefresh: browser.refresh,
    },
  ), [newFolder, browser]);

  // Menubar dropdowns. Favorites/Tools/Help are label-only chrome.
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
        { id: "cut", label: "Cut", onSelect: () => selected && browser.setClipboard({ mode: "cut", path: joinPath(path, selected) }) },
        { id: "copy", label: "Copy", onSelect: () => selected && browser.setClipboard({ mode: "copy", path: joinPath(path, selected) }) },
        { id: "paste", label: "Paste", onSelect: browser.paste },
      ],
    },
    {
      label: "View",
      items: [
        { id: "icons", label: "Icons", onSelect: () => setViewMode("icons") },
        { id: "details", label: "Details", onSelect: () => setViewMode("details") },
        { id: "refresh", label: "Refresh", onSelect: browser.refresh },
      ],
    },
    { label: "Favorites", items: null },
    { label: "Tools", items: null },
    { label: "Help", items: null },
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

  return (
    <div className="fm" onClick={closeMenu}>
      <MenuBar
        prefix="fm" menus={menus} labelX={MENU_LABEL_X} stride={MENU_LABEL_STRIDE} top={MENUBAR_TOP}
        bundle={hover.bundle} cls={hover.cls} openMenu={openMenu}
      />

      <div className="fm__toolbar">
        <TbButton prefix="fm" id="back" icon="assets/tb-back.png" label="Back" disabled={!browser.canBack} bundle={hover.bundle} cls={hover.cls} onClick={browser.back}/>
        <TbButton prefix="fm" id="forward" icon="assets/tb-forward.png" label="Forward" disabled={!browser.canForward} bundle={hover.bundle} cls={hover.cls} onClick={browser.forward}/>
        <TbButton prefix="fm" id="up" icon="assets/tb-up.png" disabled={atRoot} bundle={hover.bundle} cls={hover.cls} onClick={browser.up}/>
        <TbSeparator prefix="fm"/>
        <TbButton prefix="fm" id="folders" icon="assets/tb-folders.png" label="New Folder" bundle={hover.bundle} cls={hover.cls} onClick={newFolder}/>
        <TbButton prefix="fm" id="views" icon="assets/tb-views.png" label="Views" bundle={hover.bundle} cls={hover.cls} onClick={() => setViewMode((mode: ViewMode) => (mode === "icons" ? "details" : "icons"))}/>
      </div>

      <AddressBar
        prefix="fm" label="Address" icon="assets/folder-16.png" text={path}
        draft={browser.addressDraft}
        onBeginEdit={() => browser.setAddressDraft(path)}
        onDraftChange={browser.setAddressDraft}
        onCommit={() => browser.navigate(browser.addressDraft ?? path)}
        onCancel={() => browser.setAddressDraft(null)}
        dropItems={ancestors}
        go={{ label: "Go", icon: "assets/tb-forward.png", onClick: browser.refresh }}
        bundle={hover.bundle} cls={hover.cls} openMenu={openMenu}
      />

      <div className="fm__body">
        <div className="fm__taskpane">
          <GroupBox prefix="fm" id="tasks" title="File and Folder Tasks" expanded={expanded.tasks} onToggle={() => toggleGroup("tasks")} bundle={hover.bundle}>
            <TaskLink prefix="fm" id="newfolder" label="Make a new folder" bundle={hover.bundle} cls={hover.cls} onClick={newFolder}/>
            {selectedEntry && <TaskLink prefix="fm" id="rename" label="Rename this item" bundle={hover.bundle} cls={hover.cls} onClick={() => browser.beginRename(selectedEntry.name)}/>}
            {selectedEntry && <TaskLink prefix="fm" id="delete" label="Delete this item" bundle={hover.bundle} cls={hover.cls} onClick={() => browser.deleteEntry(selectedEntry)}/>}
          </GroupBox>
          <GroupBox prefix="fm" id="places" title="Other Places" expanded={expanded.places} onToggle={() => toggleGroup("places")} bundle={hover.bundle}>
            {!atRoot && (
              <TaskLink prefix="fm" id="up" label={parentPath(path)} bundle={hover.bundle} cls={hover.cls} onClick={browser.up}/>
            )}
            <TaskLink prefix="fm" id="root" label="My Computer" bundle={hover.bundle} cls={hover.cls} onClick={() => browser.navigate("/")}/>
          </GroupBox>
        </div>

        <FolderView
          prefix="fm" viewMode={viewMode} entries={entries} error={error}
          iconLarge={iconFor} iconSmall={iconFor16}
          entryType={(entry) => typeLabel(entry, TYPE_LABELS)}
          columns={{ name: "Name", size: "Size", type: "Type" }}
          selected={selected} renaming={browser.renaming} renameDraft={browser.renameDraft}
          onSelect={(entry) => setSelected(entry.name)}
          onOpen={browser.openEntry}
          onEntryMenu={(entry, x, y) => { setSelected(entry.name); openMenu(x, y, rowMenu(entry)); }}
          onBlankMenu={(x, y) => openMenu(x, y, emptyMenu())}
          onRenameDraftChange={browser.setRenameDraft}
          onRenameCommit={browser.commitRename}
          onRenameCancel={browser.cancelRename}
          bundle={hover.bundle} cls={hover.cls}
        />
      </div>

      <div className="fm__status">
        <span>{entries.length} objects</span>
      </div>

      {menu && <ContextMenu x={menu.x} y={menu.y} items={menu.items} onClose={closeMenu}/>}
    </div>
  );
}
