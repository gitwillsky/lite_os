import React, { useCallback, useMemo, useState } from "react";
import type { FsEntry } from "lite:fs";
import { ContextMenu } from "../design-system/context-menu.tsx";
import { PropertiesPopup } from "../design-system/properties-popup.tsx";
import { baseName, formatSize, joinPath, parentPath, typeLabel } from "../explorer/model.ts";
import type { TypeLabels } from "../explorer/model.ts";
import { fsListing, useBrowser } from "../explorer/use-browser.ts";
import type { Listing } from "../explorer/use-browser.ts";
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

/** An open dropdown/context menu: viewport-local position and its rows. */
interface MenuState {
  x: number;
  y: number;
  items: MenuItem[];
}

/** An open properties popup: viewport-local position, title and rows. */
interface PopupState {
  x: number;
  y: number;
  title: string;
  rows: [string, string][];
}

// The machine story is static and honest: QEMU attaches exactly one virtio-blk
// disk (the ext2 rootfs, volume label LITEOS) and no optical/floppy drive, so
// 我的电脑 shows a single 本地磁盘 (C:) and no removable-devices group. The
// virtual root is the empty path ""; double-clicking C: enters "/" in the SAME
// window (XP's default "open each folder in the same window"), and from there
// the shared explorer core provides full browsing.
const VIRTUAL_ROOT = "";
const DRIVE_ENTRIES: FsEntry[] = [{ name: "本地磁盘 (C:)", kind: "dir", size: 0 }];

function listEntries(path: string): Listing {
  return path === VIRTUAL_ROOT ? { entries: DRIVE_ENTRIES, error: null } : fsListing(path);
}

/** Up from "/" returns to the virtual 我的电脑 root; the root itself is a
 * fixed point so Up disables there (parentPath("") would wrongly give "/"). */
function parentOf(path: string): string {
  if (path === VIRTUAL_ROOT) return VIRTUAL_ROOT;
  return path === "/" ? VIRTUAL_ROOT : parentPath(path);
}

function openTarget(path: string, entry: FsEntry): string | null {
  if (path === VIRTUAL_ROOT) return "/";
  return entry.kind === "dir" || entry.kind === "symlink" ? joinPath(path, entry.name) : null;
}

const TYPE_LABELS: TypeLabels = {
  folder: "文件夹",
  shortcut: "快捷方式",
  file: "文件",
  extensionFile: (extension) => `${extension} 文件`,
};

function iconFor(entry: FsEntry): string {
  return entry.kind === "dir" || entry.kind === "symlink"
    ? "assets/folder.png"
    : "assets/file.png";
}

function iconFor16(entry: FsEntry): string {
  return entry.kind === "dir" || entry.kind === "symlink"
    ? "assets/folder-16.png"
    : "assets/file-16.png";
}

// evdev KEY_BACKSPACE: XP maps it to Up when no text field is focused.
const KEY_BACKSPACE = 14;
// Fixed menubar geometry: dropdowns open just under the clicked label. The
// labels are wider than file-manager's English ones (CJK + access key), so
// the stride grows accordingly.
const MENUBAR_TOP = 40;
const MENU_LABEL_X = 4;
const MENU_LABEL_STRIDE = 72;
// 查看 toolbar button sits after three labeled buttons and a separator; its
// dropdown opens at this fixed offset (same hardcoded-geometry pattern as the
// menubar).
const VIEWS_MENU_X = 246;
const VIEWS_MENU_Y = 62;

export default function MyComputer() {
  const browser = useBrowser(VIRTUAL_ROOT, { listEntries, parentOf, openTarget });
  const { path, entries, error, viewMode, setViewMode, selected, setSelected } = browser;
  const { hover } = browser;
  const [statusVisible, setStatusVisible] = useState(true);
  const [expanded, setExpanded] = useState<Record<string, boolean>>({
    tasks: true,
    places: true,
    details: true,
  });
  const [menu, setMenu] = useState<MenuState | null>(null);
  const [popup, setPopup] = useState<PopupState | null>(null);

  const closeOverlays = useCallback(() => { setMenu(null); setPopup(null); }, []);
  const openMenu = useCallback((x: number, y: number, items: MenuItem[]) => {
    setPopup(null);
    setMenu({ x, y, items });
  }, []);
  const openPopup = useCallback((x: number, y: number, title: string, rows: [string, string][]) => {
    setMenu(null);
    setPopup({ x, y, title, rows });
  }, []);

  const atRoot = path === VIRTUAL_ROOT;
  const selectedEntry = entries.find((entry) => entry.name === selected) ?? null;
  const newFolder = useCallback(() => browser.newFolder("新建文件夹"), [browser]);
  const toggleGroup = (id: string) =>
    setExpanded((current) => ({ ...current, [id]: !current[id] }));

  const driveProperties = useCallback((x: number, y: number) =>
    openPopup(x, y, "本地磁盘 (C:) 属性", [["类型", "本地磁盘"], ["文件系统", "ext2"]]), [openPopup]);
  const computerProperties = useCallback((x: number, y: number) =>
    openPopup(x, y, "我的电脑 属性", [["系统", "LiteOS"], ["磁盘", "本地磁盘 (C:)"], ["文件系统", "ext2"]]), [openPopup]);
  const entryProperties = useCallback((entry: FsEntry, x: number, y: number) => {
    const rows: [string, string][] = [
      ["类型", typeLabel(entry, TYPE_LABELS)],
      ["位置", path],
    ];
    if (entry.kind !== "dir" && entry.kind !== "symlink") rows.push(["大小", formatSize(entry)]);
    openPopup(x, y, `${entry.name} 属性`, rows);
  }, [openPopup, path]);

  const viewItems = useCallback((): MenuItem[] => [
    { id: "icons", label: "大图标(G)", onSelect: () => setViewMode("icons") },
    { id: "list", label: "列表(L)", onSelect: () => setViewMode("list") },
    { id: "details", label: "详细信息(D)", onSelect: () => setViewMode("details") },
  ], [setViewMode]);

  // Row context menu: drives expose Open/Properties only (no fs mutations at
  // the virtual root); real entries get the full explorer verb set.
  const rowMenu = useCallback((entry: FsEntry, x: number, y: number): MenuItem[] => {
    if (atRoot) {
      return [
        { id: "open", label: "打开(O)", onSelect: () => browser.openEntry(entry) },
        { id: "properties", label: "属性(R)", onSelect: () => driveProperties(x, y) },
      ];
    }
    return entryMenu(
      { open: "打开(O)", cut: "剪切(T)", copy: "复制(C)", delete: "删除(D)", rename: "重命名(M)", properties: "属性(R)" },
      {
        onOpen: () => browser.openEntry(entry),
        onCut: () => browser.setClipboard({ mode: "cut", path: joinPath(path, entry.name) }),
        onCopy: () => browser.setClipboard({ mode: "copy", path: joinPath(path, entry.name) }),
        onDelete: () => browser.deleteEntry(entry),
        onRename: () => browser.beginRename(entry.name),
        onProperties: () => entryProperties(entry, x, y),
      },
    );
  }, [atRoot, browser, path, driveProperties, entryProperties]);

  // Blank-area context menu: view switchers everywhere; fs verbs only in a
  // real folder.
  const emptyMenu = useCallback((x: number, y: number): MenuItem[] => {
    const items = viewItems();
    if (!atRoot) {
      items.push(...blankMenu(
        { newFolder: "新建文件夹(F)", paste: "粘贴(P)", refresh: "刷新(R)" },
        {
          onNewFolder: newFolder,
          onPaste: browser.clipboard ? browser.paste : undefined,
          onRefresh: browser.refresh,
        },
      ));
    } else {
      items.push({ id: "refresh", label: "刷新(R)", onSelect: browser.refresh });
    }
    items.push({ id: "properties", label: "属性(R)", onSelect: () => atRoot ? computerProperties(x, y) : openPopup(x, y, `${path} 属性`, [["位置", path], ["对象", String(entries.length)]]) });
    return items;
  }, [viewItems, atRoot, newFolder, browser, computerProperties, openPopup, path, entries.length]);

  // Menubar dropdowns. 收藏/工具 stay label-only chrome, matching
  // file-manager's convention for menus without honest targets. 编辑 items are
  // included only when applicable — a menu of dead rows would fake function.
  const editItems: MenuItem[] = [];
  if (!atRoot && selected) {
    editItems.push(
      { id: "cut", label: "剪切(T)", onSelect: () => browser.setClipboard({ mode: "cut", path: joinPath(path, selected) }) },
      { id: "copy", label: "复制(C)", onSelect: () => browser.setClipboard({ mode: "copy", path: joinPath(path, selected) }) },
    );
  }
  if (!atRoot && browser.clipboard) {
    editItems.push({ id: "paste", label: "粘贴(P)", onSelect: browser.paste });
  }
  const menus: { label: string; items: MenuItem[] | null }[] = [
    {
      label: "文件(F)",
      items: [
        ...(!atRoot ? [{ id: "new", label: "新建文件夹(F)", onSelect: newFolder }] : []),
        ...(!atRoot && selectedEntry ? [
          { id: "rename", label: "重命名(M)", onSelect: () => browser.beginRename(selectedEntry.name) },
          { id: "delete", label: "删除(D)", onSelect: () => browser.deleteEntry(selectedEntry) },
        ] : []),
        { id: "properties", label: "属性(R)", onSelect: () => atRoot
          ? computerProperties(MENU_LABEL_X, MENUBAR_TOP + 18)
          : selectedEntry
            ? entryProperties(selectedEntry, MENU_LABEL_X, MENUBAR_TOP + 18)
            : openPopup(MENU_LABEL_X, MENUBAR_TOP + 18, `${path} 属性`, [["位置", path], ["对象", String(entries.length)]]) },
      ],
    },
    {
      label: "编辑(E)",
      items: editItems.length > 0 ? editItems : null,
    },
    {
      label: "查看(V)",
      items: [
        ...viewItems(),
        { id: "status", label: "状态栏(B)", onSelect: () => setStatusVisible((visible) => !visible) },
        { id: "refresh", label: "刷新(R)", onSelect: browser.refresh },
      ],
    },
    { label: "收藏(A)", items: null },
    { label: "工具(T)", items: null },
    {
      label: "帮助(H)",
      items: [
        { id: "about", label: "关于我的电脑(A)", onSelect: () => openPopup(MENU_LABEL_X + 5 * MENU_LABEL_STRIDE, MENUBAR_TOP + 18, "关于我的电脑", [["名称", "我的电脑"], ["系统", "LiteOS"]]) },
      ],
    },
  ];

  // Address caret: 我的电脑 plus every ancestor directory, each navigable.
  const ancestors = useMemo(() => {
    const list: MenuItem[] = [{ id: VIRTUAL_ROOT, label: "我的电脑", onSelect: () => browser.navigate(VIRTUAL_ROOT) }];
    let acc = "";
    for (const part of path.split("/").filter(Boolean)) {
      acc = `${acc}/${part}`;
      const full = acc;
      list.push({ id: full, label: full, onSelect: () => browser.navigate(full) });
    }
    return list;
  }, [path, browser]);

  // Status bar: object count, or the selection and its size like XP.
  const statusText = selectedEntry
    ? `选定了 1 个对象${selectedEntry.kind === "file" ? `  ${formatSize(selectedEntry)}` : ""}`
    : `${entries.length} 个对象`;
  const placeText = atRoot ? "我的电脑" : baseName(path) || "/";
  const detailName = atRoot ? "我的电脑" : baseName(path) || path;

  return (
    <div
      className="mc"
      onClick={() => { closeOverlays(); setSelected(null); }}
      onKeyDown={(rawEvent) => {
        const key = rawEvent as unknown as { code: number; value: number };
        if (key.code === KEY_BACKSPACE && key.value !== 0 && browser.canUp) browser.up();
      }}
    >
      <MenuBar
        prefix="mc" menus={menus} labelX={MENU_LABEL_X} stride={MENU_LABEL_STRIDE} top={MENUBAR_TOP}
        bundle={hover.bundle} cls={hover.cls} openMenu={openMenu}
      />

      <div className="mc__toolbar">
        <TbButton prefix="mc" id="back" icon="assets/tb-back.png" label="后退" disabled={!browser.canBack} bundle={hover.bundle} cls={hover.cls} onClick={browser.back}/>
        <TbButton prefix="mc" id="forward" icon="assets/tb-forward.png" label="前进" disabled={!browser.canForward} bundle={hover.bundle} cls={hover.cls} onClick={browser.forward}/>
        <TbButton prefix="mc" id="up" icon="assets/tb-up.png" label="向上" disabled={!browser.canUp} bundle={hover.bundle} cls={hover.cls} onClick={browser.up}/>
        <TbSeparator prefix="mc"/>
        <TbButton prefix="mc" id="views" icon="assets/tb-views.png" label="查看" bundle={hover.bundle} cls={hover.cls} onClick={() => openMenu(VIEWS_MENU_X, VIEWS_MENU_Y, viewItems())}/>
      </div>

      <AddressBar
        prefix="mc" label="地址(D)" icon={atRoot ? "assets/computer.png" : "assets/folder-16.png"}
        text={atRoot ? "我的电脑" : path}
        draft={browser.addressDraft}
        onBeginEdit={() => browser.setAddressDraft(path)}
        onDraftChange={browser.setAddressDraft}
        onCommit={() => browser.navigate(browser.addressDraft ?? path)}
        onCancel={() => browser.setAddressDraft(null)}
        dropItems={ancestors}
        bundle={hover.bundle} cls={hover.cls} openMenu={openMenu}
      />

      <div className="mc__body">
        <div className="mc__taskpane">
          {!atRoot && (
            <GroupBox prefix="mc" id="tasks" title="文件和文件夹任务" expanded={expanded.tasks} onToggle={() => toggleGroup("tasks")} bundle={hover.bundle}>
              <TaskLink prefix="mc" id="newfolder" label="新建一个文件夹" bundle={hover.bundle} cls={hover.cls} onClick={newFolder}/>
              <TaskLink prefix="mc" id="rename" label="重命名这个项目" disabled={!selectedEntry} bundle={hover.bundle} cls={hover.cls} onClick={() => selectedEntry && browser.beginRename(selectedEntry.name)}/>
              <TaskLink prefix="mc" id="delete" label="删除这个项目" disabled={!selectedEntry} bundle={hover.bundle} cls={hover.cls} onClick={() => selectedEntry && browser.deleteEntry(selectedEntry)}/>
              <TaskLink prefix="mc" id="copy" label="复制这个项目" disabled={!selectedEntry} bundle={hover.bundle} cls={hover.cls} onClick={() => selectedEntry && browser.setClipboard({ mode: "copy", path: joinPath(path, selectedEntry.name) })}/>
            </GroupBox>
          )}
          {!atRoot && (
            <GroupBox prefix="mc" id="places" title="其他位置" expanded={expanded.places} onToggle={() => toggleGroup("places")} bundle={hover.bundle}>
              <TaskLink prefix="mc" id="computer" label="我的电脑" bundle={hover.bundle} cls={hover.cls} onClick={() => browser.navigate(VIRTUAL_ROOT)}/>
            </GroupBox>
          )}
          <GroupBox prefix="mc" id="details" title="详细信息" expanded={expanded.details} onToggle={() => toggleGroup("details")} bundle={hover.bundle}>
            {selectedEntry ? (
              <>
                <img className="mc__detail-icon" src={atRoot ? "assets/drive.png" : iconFor(selectedEntry)}/>
                <span className="mc__detail-name">{selectedEntry.name}</span>
                <span className="mc__detail-line">类型： {atRoot ? "本地磁盘" : typeLabel(selectedEntry, TYPE_LABELS)}</span>
                {atRoot && <span className="mc__detail-line">文件系统： ext2</span>}
                {!atRoot && selectedEntry.kind === "file" && <span className="mc__detail-line">大小： {formatSize(selectedEntry)}</span>}
              </>
            ) : (
              <>
                <img className="mc__detail-icon" src={atRoot ? "assets/computer.png" : "assets/folder.png"}/>
                <span className="mc__detail-name">{detailName}</span>
                <span className="mc__detail-line">{atRoot ? "选择要查看的项目。" : `${entries.length} 个对象`}</span>
              </>
            )}
          </GroupBox>
        </div>

        <FolderView
          prefix="mc" viewMode={viewMode} entries={entries} error={error}
          iconLarge={atRoot ? () => "assets/drive.png" : iconFor}
          iconSmall={atRoot ? () => "assets/drive-16.png" : iconFor16}
          entryType={(entry) => atRoot ? "本地磁盘" : typeLabel(entry, TYPE_LABELS)}
          columns={{ name: "名称", size: "大小", type: "类型" }}
          selected={selected} renaming={browser.renaming} renameDraft={browser.renameDraft}
          onSelect={(entry) => setSelected(entry.name)}
          onOpen={browser.openEntry}
          onEntryMenu={(entry, x, y) => { setSelected(entry.name); openMenu(x, y, rowMenu(entry, x, y)); }}
          onBlankMenu={(x, y) => openMenu(x, y, emptyMenu(x, y))}
          onBlankClick={() => setSelected(null)}
          heading={atRoot ? "硬盘" : undefined}
          onRenameDraftChange={browser.setRenameDraft}
          onRenameCommit={browser.commitRename}
          onRenameCancel={browser.cancelRename}
          bundle={hover.bundle} cls={hover.cls}
        />
      </div>

      {statusVisible && (
        <div className="mc__status">
          <span className="mc__status-cell">{statusText}</span>
          <span className="mc__status-cell mc__status-place">
            <img className="mc__status-icon" src={atRoot ? "assets/computer.png" : "assets/folder-16.png"}/>
            <span>{placeText}</span>
          </span>
        </div>
      )}

      {menu && <ContextMenu x={menu.x} y={menu.y} items={menu.items} onClose={closeOverlays}/>}
      {popup && <PropertiesPopup x={popup.x} y={popup.y} title={popup.title} rows={popup.rows} onClose={closeOverlays}/>}
    </div>
  );
}
