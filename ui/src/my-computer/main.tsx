import React, { useCallback, useMemo, useState } from "react";
import { read } from "lite:fs";
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
  baseName,
  formatDate,
  formatSize,
  joinPath,
  parentPath,
  typeLabel,
} from "../explorer/model.ts";
import type { TypeLabels } from "../explorer/model.ts";
import { fsListing, useBrowser } from "../explorer/use-browser.ts";
import type { Listing, SortColumn } from "../explorer/use-browser.ts";
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
  PropertiesDialog,
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

/** The one open modal, or null. */
type DialogState =
  | { kind: "viewer"; view: FileView }
  | { kind: "cannot-open"; name: string }
  | { kind: "options" }
  | { kind: "properties"; title: string; rows: [string, string][] }
  | null;

// The machine story is static and honest: QEMU attaches exactly one virtio-blk
// disk (the ext2 rootfs, volume label LITEOS) and no optical/floppy drive, so
// 我的电脑 shows a single 本地磁盘 (C:) and no removable-devices group. The
// virtual root is the empty path ""; double-clicking C: enters "/" in the SAME
// window, and from there
// the shared explorer core provides full browsing.
const VIRTUAL_ROOT = "";
const DRIVE_ENTRIES: FsEntry[] = [{ name: "本地磁盘 (C:)", kind: "dir", size: 0, mtime: 0 }];

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

/** Total of several selected files formatted for the shared status bar. */
function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${Math.round(bytes / (1024 * 1024))} MB`;
}

/** Parses one "Name:   12345 kB" meminfo line into MB text, or null. */
function meminfoMb(content: string, key: string): string | null {
  const line = content.split("\n").find((row) => row.startsWith(`${key}:`));
  const kb = line ? Number(line.replace(/[^0-9]/g, "")) : NaN;
  return Number.isFinite(kb) && kb > 0 ? `${Math.round(kb / 1024)} MB` : null;
}

/** 系统任务 → 查看系统信息: every row comes from the real procfs (mounted at
 * /proc per the boot log) or a static machine fact; rows that fail to read
 * are dropped rather than faked. */
function systemInfoRows(): [string, string][] {
  const rows: [string, string][] = [["系统", "LiteOS"]];
  const stat = read("/proc/stat");
  if (!stat.error && stat.content) {
    const cpus = stat.content.split("\n").filter((line) => /^cpu[0-9]/.test(line)).length;
    if (cpus > 0) rows.push(["处理器", `${cpus} 个逻辑 CPU`]);
  }
  const meminfo = read("/proc/meminfo");
  if (!meminfo.error && meminfo.content) {
    const total = meminfoMb(meminfo.content, "MemTotal");
    const available = meminfoMb(meminfo.content, "MemAvailable");
    if (total) rows.push(["内存", available ? `${total} / 可用 ${available}` : total]);
  }
  const loadavg = read("/proc/loadavg");
  if (!loadavg.error && loadavg.content) {
    const fields = loadavg.content.trim().split(/\s+/);
    if (fields.length >= 3) rows.push(["负载", `${fields[0]} / ${fields[1]} / ${fields[2]}`]);
  }
  const uptime = read("/proc/uptime");
  if (!uptime.error && uptime.content) {
    const seconds = Math.floor(Number(uptime.content.trim().split(/\s+/)[0]));
    if (Number.isFinite(seconds)) {
      const hours = Math.floor(seconds / 3600);
      const minutes = Math.floor((seconds % 3600) / 60);
      rows.push(["运行时间", hours > 0 ? `${hours} 小时 ${minutes} 分钟` : `${minutes} 分钟 ${seconds % 60} 秒`]);
    }
  }
  return rows;
}

// Menubar geometry: CJK labels + access keys need a wider stride than
// file-manager's English ones.
const MENU_LABEL_X = 4;
const MENU_LABEL_STRIDE = 72;
// Back/Forward history dropdowns open under their toolbar buttons; the 查看
// menu under its button (same hardcoded-geometry pattern as the menubar).
const BACK_MENU_X = 8;
const FORWARD_MENU_X = 92;
const NAV_MENU_Y = 64;
const VIEWS_MENU_X = 300;

export default function MyComputer() {
  const [dialog, setDialog] = useState<DialogState>(null);
  const closeDialog = useCallback(() => setDialog(null), []);

  const openFile = useCallback((path: string, entry: FsEntry) => {
    const result = readTextFile(path, entry);
    if ("view" in result) setDialog({ kind: "viewer", view: result.view });
    else setDialog({ kind: "cannot-open", name: entry.name });
  }, []);

  const browser = useBrowser(VIRTUAL_ROOT, { typeLabels: TYPE_LABELS, listEntries, parentOf, openTarget, onOpenFile: openFile });
  const { path, entries, error, viewMode, setViewMode, selected } = browser;
  const [statusVisible, setStatusVisible] = useState(true);
  const [foldersPane, setFoldersPane] = useState(false);
  const [expanded, setExpanded] = useState<Record<string, boolean>>({
    system: true,
    tasks: true,
    places: true,
    details: true,
  });
  const [menu, setMenu] = useState<MenuState | null>(null);

  const closeMenu = useCallback(() => setMenu(null), []);
  const openMenu = useCallback((x: number, y: number, items: MenuItem[]) => {
    setMenu({ x, y, items });
  }, []);
  const openProperties = useCallback((title: string, rows: [string, string][]) => {
    setMenu(null);
    setDialog({ kind: "properties", title, rows });
  }, []);

  const atRoot = path === VIRTUAL_ROOT;
  const selectedEntries = useMemo(
    () => entries.filter((entry) => selected.includes(entry.name)),
    [entries, selected],
  );
  const focusedEntry = selectedEntries.at(-1) ?? null;
  const newFolder = useCallback(() => browser.newFolder("新建文件夹"), [browser]);
  const toggleGroup = (id: string) =>
    setExpanded((current) => ({ ...current, [id]: !current[id] }));

  const driveProperties = useCallback(() =>
    openProperties("本地磁盘 (C:) 属性", [["类型", "本地磁盘"], ["文件系统", "ext2"]]), [openProperties]);
  const computerProperties = useCallback(() =>
    openProperties("我的电脑 属性", [["系统", "LiteOS"], ["磁盘", "本地磁盘 (C:)"], ["文件系统", "ext2"]]), [openProperties]);
  const entryProperties = useCallback((entry: FsEntry) => {
    const rows: [string, string][] = [
      ["类型", typeLabel(entry, TYPE_LABELS)],
      ["位置", path],
    ];
    if (entry.kind !== "dir" && entry.kind !== "symlink") rows.push(["大小", formatSize(entry)]);
    if (entry.mtime > 0) rows.push(["修改时间", formatDate(entry.mtime)]);
    openProperties(`${entry.name} 属性`, rows);
  }, [openProperties, path]);

  const viewItems = useCallback((): MenuItem[] => [
    { id: "icons", label: "大图标(G)", onSelect: () => setViewMode("icons") },
    { id: "list", label: "列表(L)", onSelect: () => setViewMode("list") },
    { id: "details", label: "详细信息(D)", onSelect: () => setViewMode("details") },
  ], [setViewMode]);

  // Back/Forward history dropdowns live beside their navigation buttons.
  const historyMenu = useCallback((direction: "back" | "forward"): MenuItem[] => {
    const { history, historyIndex } = browser;
    const range = direction === "back"
      ? history.slice(0, historyIndex).map((entry, index) => ({ entry, index })).reverse()
      : history.slice(historyIndex + 1).map((entry, offset) => ({ entry, index: historyIndex + 1 + offset }));
    return range.map(({ entry, index }) => ({
      id: String(index),
      label: entry === VIRTUAL_ROOT ? "我的电脑" : entry,
      onSelect: () => browser.jumpTo(index),
    }));
  }, [browser]);

  // Row context menu: drives expose Open/Properties only (no fs mutations at
  // the virtual root); real entries get the full explorer verb set.
  const rowMenu = useCallback((entry: FsEntry): MenuItem[] => {
    if (atRoot) {
      return [
        { id: "open", label: "打开(O)", onSelect: () => browser.openEntry(entry) },
        { id: "properties", label: "属性(R)", onSelect: driveProperties },
      ];
    }
    return entryMenu(
      { open: "打开(O)", cut: "剪切(T)", copy: "复制(C)", delete: "删除(D)", rename: "重命名(M)", properties: "属性(R)" },
      {
        onOpen: () => browser.openEntry(entry),
        onCut: () => browser.clipboardFromSelection("cut", selectedEntries),
        onCopy: () => browser.clipboardFromSelection("copy", selectedEntries),
        onDelete: () => browser.deleteSelected(selectedEntries),
        onRename: () => browser.beginRename(entry.name),
        onProperties: () => entryProperties(entry),
      },
    );
  }, [atRoot, browser, selectedEntries, driveProperties, entryProperties]);

  // Blank-area context menu: view switchers everywhere; fs verbs only in a
  // real folder.
  const emptyMenu = useCallback((): MenuItem[] => {
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
    items.push({ id: "properties", label: "属性(R)", onSelect: () => atRoot ? computerProperties() : openProperties(`${path} 属性`, [["位置", path], ["对象", String(entries.length)]]) });
    return items;
  }, [viewItems, atRoot, newFolder, browser, computerProperties, openProperties, path, entries.length]);

  // Menubar dropdowns. 收藏 was removed (lite:fs has no write API to persist
  // favorites — label-only chrome would fake function); 编辑 items appear only
  // when applicable for the same reason.
  const editItems: MenuItem[] = [];
  if (!atRoot && selected.length > 0) {
    editItems.push(
      { id: "cut", label: "剪切(T)", onSelect: () => browser.clipboardFromSelection("cut", selectedEntries) },
      { id: "copy", label: "复制(C)", onSelect: () => browser.clipboardFromSelection("copy", selectedEntries) },
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
        ...(!atRoot && focusedEntry ? [
          { id: "rename", label: "重命名(M)", onSelect: () => browser.beginRename(focusedEntry.name) },
          { id: "delete", label: "删除(D)", onSelect: () => browser.deleteSelected(selectedEntries) },
        ] : []),
        { id: "properties", label: "属性(R)", onSelect: () => atRoot
          ? computerProperties()
          : focusedEntry
            ? entryProperties(focusedEntry)
            : openProperties(`${path} 属性`, [["位置", path], ["对象", String(entries.length)]]) },
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
    {
      label: "工具(T)",
      items: [
        { id: "options", label: "文件夹选项(O)", onSelect: () => setDialog({ kind: "options" }) },
      ],
    },
    {
      label: "帮助(H)",
      items: [
        { id: "about", label: "关于我的电脑(A)", onSelect: () => openProperties("关于我的电脑", [["名称", "我的电脑"], ["系统", "LiteOS"]]) },
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

  // Explorer keyboard map on the global onKeyDown path; a focused input (rename,
  // address) captures its own keys first. Fs verbs are guarded at the virtual
  // root, where there is nothing to mutate.
  const onKeyDown = useCallback((rawEvent: unknown) => {
    const key = rawEvent as unknown as LiteKeyEvent;
    if (key.value === 0) return;
    if (dialog) {
      if (key.code === KEY_ESC) closeDialog();
      return;
    }
    const control = (key.modifiers & MOD_CONTROL) !== 0;
    if (control && key.code === KEY_A) browser.selectAll();
    else if (control && key.code === KEY_X) { if (!atRoot) browser.clipboardFromSelection("cut", selectedEntries); }
    else if (control && key.code === KEY_C) { if (!atRoot) browser.clipboardFromSelection("copy", selectedEntries); }
    else if (control && key.code === KEY_V) { if (!atRoot) browser.paste(); }
    else if (key.code === KEY_BACKSPACE) { if (browser.canUp) browser.up(); }
    else if (key.code === KEY_ENTER && focusedEntry) browser.openEntry(focusedEntry);
    else if (key.code === KEY_F2 && focusedEntry && !atRoot) browser.beginRename(focusedEntry.name);
    else if (key.code === KEY_DELETE && selectedEntries.length > 0 && !atRoot) browser.deleteSelected(selectedEntries);
  }, [dialog, closeDialog, browser, selectedEntries, focusedEntry, atRoot]);

  // Status bar: object count, or the selection and its total size.
  const selectedBytes = selectedEntries.reduce((sum, entry) => sum + (entry.kind === "file" ? entry.size : 0), 0);
  const statusText = selected.length > 0
    ? `选定了 ${selected.length} 个对象${selectedBytes > 0 ? `  ${formatBytes(selectedBytes)}` : ""}`
    : `${entries.length} 个对象`;
  const placeText = atRoot ? "我的电脑" : baseName(path) || "/";
  const detailName = atRoot ? "我的电脑" : baseName(path) || path;

  return (
    <div
      className="aurora-root explorer"
      onClick={() => { closeMenu(); browser.clearSelection(); }}
      onKeyDown={onKeyDown}
    >
      <MenuBar menus={menus} labelX={MENU_LABEL_X} stride={MENU_LABEL_STRIDE}/>

      <Toolbar>
        <ToolbarButton icon="assets/nav-back.png" label="后退" disabled={!browser.canBack} dropdown={{ items: historyMenu("back"), at: { x: BACK_MENU_X, y: NAV_MENU_Y } }} onClick={browser.back}/>
        <ToolbarButton icon="assets/nav-forward.png" label="前进" disabled={!browser.canForward} dropdown={{ items: historyMenu("forward"), at: { x: FORWARD_MENU_X, y: NAV_MENU_Y } }} onClick={browser.forward}/>
        <ToolbarButton icon="assets/nav-up.png" label="向上" disabled={!browser.canUp} onClick={browser.up}/>
        <ToolbarSeparator/>
        <ToolbarButton icon="assets/files.png" label="文件夹" onClick={() => setFoldersPane((value) => !value)}/>
        <ToolbarButton icon="assets/view-grid.png" label="查看" dropdown={{ items: viewItems(), at: { x: VIEWS_MENU_X, y: NAV_MENU_Y } }}/>
      </Toolbar>

      <AddressBar
        label="地址(D)" icon={atRoot ? "assets/package.png" : "assets/folder-16.png"}
        text={atRoot ? "我的电脑" : path}
        draft={browser.addressDraft}
        onBeginEdit={() => browser.setAddressDraft(path)}
        onDraftChange={browser.setAddressDraft}
        onCommit={() => browser.navigate(browser.addressDraft ?? path)}
        onCancel={() => browser.setAddressDraft(null)}
        dropItems={ancestors}
      />

      <div className="explorer__body">
        {foldersPane ? (
          <FolderTree
            roots={[
              { path: VIRTUAL_ROOT, label: "我的电脑", icon: "assets/package.png" },
              { path: "/", label: "本地磁盘 (C:)", icon: "assets/drive-16.png" },
            ]}
            currentPath={path}
            listDirs={(dir) => (dir === VIRTUAL_ROOT ? [] : subdirs(dir))}
            onNavigate={browser.navigate}
          />
        ) : (
          <div className="task-pane">
            {atRoot && (
              <GroupBox title="系统任务" expanded={expanded.system} onToggle={() => toggleGroup("system")}>
                <TaskLink label="查看系统信息" onClick={() => openProperties("系统信息", systemInfoRows())}/>
              </GroupBox>
            )}
            {!atRoot && (
              <GroupBox title="文件和文件夹任务" expanded={expanded.tasks} onToggle={() => toggleGroup("tasks")}>
                <TaskLink label="新建一个文件夹" onClick={newFolder}/>
                <TaskLink label="重命名这个项目" disabled={!focusedEntry} onClick={() => focusedEntry && browser.beginRename(focusedEntry.name)}/>
                <TaskLink label="删除这个项目" disabled={selectedEntries.length === 0} onClick={() => browser.deleteSelected(selectedEntries)}/>
                <TaskLink label="复制这个项目" disabled={selectedEntries.length === 0} onClick={() => browser.clipboardFromSelection("copy", selectedEntries)}/>
              </GroupBox>
            )}
            {!atRoot && (
              <GroupBox title="其他位置" expanded={expanded.places} onToggle={() => toggleGroup("places")}>
                <TaskLink label="我的电脑" onClick={() => browser.navigate(VIRTUAL_ROOT)}/>
              </GroupBox>
            )}
            <GroupBox title="详细信息" expanded={expanded.details} onToggle={() => toggleGroup("details")}>
              {focusedEntry ? (
                <>
                  <img className="detail-icon" src={atRoot ? "assets/drive.png" : iconFor(focusedEntry)}/>
                  <span className="detail-name">{focusedEntry.name}</span>
                  <span className="detail-line">类型： {atRoot ? "本地磁盘" : typeLabel(focusedEntry, TYPE_LABELS)}</span>
                  {atRoot && <span className="detail-line">文件系统： ext2</span>}
                  {!atRoot && focusedEntry.kind === "file" && <span className="detail-line">大小： {formatSize(focusedEntry)}</span>}
                  {!atRoot && focusedEntry.mtime > 0 && <span className="detail-line">修改时间： {formatDate(focusedEntry.mtime)}</span>}
                </>
              ) : (
                <>
                  <img className="detail-icon" src={atRoot ? "assets/package.png" : "assets/files.png"}/>
                  <span className="detail-name">{detailName}</span>
                  <span className="detail-line">{atRoot ? "选择要查看的项目。" : `${entries.length} 个对象`}</span>
                </>
              )}
            </GroupBox>
          </div>
        )}

        <FolderView
          viewMode={viewMode} entries={entries} error={error}
          iconLarge={atRoot ? () => "assets/drive.png" : iconFor}
          iconSmall={atRoot ? () => "assets/drive-16.png" : iconFor16}
          entryType={(entry) => atRoot ? "本地磁盘" : typeLabel(entry, TYPE_LABELS)}
          columns={{ name: "名称", size: "大小", type: "类型", mtime: "修改日期" }}
          formatDate={formatDate}
          sort={browser.sort} onSort={(column: SortColumn) => browser.toggleSort(column)}
          selected={selected} renaming={browser.renaming} renameDraft={browser.renameDraft} renameMaxWidth={90}
          onSelect={(entry) => browser.selectOnly(entry.name)}
          onOpen={browser.openEntry}
          onEntryMenu={(entry, x, y) => { browser.selectOnly(entry.name); openMenu(x, y, rowMenu(entry)); }}
          onBlankMenu={(x, y) => openMenu(x, y, emptyMenu())}
          onBlankClick={() => browser.clearSelection()}
          heading={atRoot ? "硬盘" : undefined}
          onRenameDraftChange={browser.setRenameDraft}
          onRenameCommit={browser.commitRename}
          onRenameCancel={browser.cancelRename}
        />
      </div>

      {statusVisible && (
        <StatusBar>
          <StatusBarCell text={statusText}/>
          <StatusBarCell icon={atRoot ? "assets/package.png" : "assets/folder-16.png"} text={placeText}/>
        </StatusBar>
      )}

      {menu && <ContextMenu x={menu.x} y={menu.y} items={menu.items} onClose={closeMenu}/>}
      {dialog?.kind === "viewer" && (
        <TextViewer view={dialog.view} onClose={closeDialog} closeLabel="关闭" truncatedLabel="（内容超过 64 KB，已截断）"/>
      )}
      {dialog?.kind === "cannot-open" && (
        <CannotOpenDialog name={dialog.name} message={(name) => `Windows 无法打开 '${name}'。没有程序与此文件类型关联。`} onClose={closeDialog} closeLabel="确定"/>
      )}
      {dialog?.kind === "options" && (
        <FolderOptionsDialog
          showHidden={browser.showHidden}
          onToggleHidden={() => browser.setShowHidden(!browser.showHidden)}
          viewMode={viewMode}
          views={[{ mode: "icons", label: "大图标" }, { mode: "list", label: "列表" }, { mode: "details", label: "详细信息" }]}
          onPickView={setViewMode}
          onClose={closeDialog}
          labels={{ hiddenFiles: "显示隐藏的文件和文件夹", view: "文件夹选项", close: "确定" }}
        />
      )}
      {dialog?.kind === "properties" && (
        <PropertiesDialog title={dialog.title} rows={dialog.rows} onClose={closeDialog} closeLabel="确定"/>
      )}
    </div>
  );
}
