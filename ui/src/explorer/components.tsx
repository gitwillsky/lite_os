import React, { useEffect, useState } from "react";
import type { FsEntry } from "lite:fs";
import { SystemIcon, TextInput } from "../design-system/controls.tsx";
import type { MenuItem } from "../design-system/controls.tsx";
import { formatSize as formatSizeFor, measureText11 } from "./model.ts";
import { fsListing } from "./use-browser.ts";
import type { SortColumn, SortState, ViewMode } from "./use-browser.ts";

export type { MenuItem };

// evdev keycodes delivered on a focused input's onKeyDown for commit/cancel.
const KEY_ESC = 1;
const KEY_ENTER = 28;
const KEY_UP = 103;
const KEY_LEFT = 105;
const KEY_RIGHT = 106;
const KEY_DOWN = 108;

function handleTreeKey(rawEvent: unknown, expanded: boolean, expandable: boolean, onToggle: () => void) {
  const event = rawEvent as unknown as LiteKeyEvent;
  if (![KEY_UP, KEY_LEFT, KEY_RIGHT, KEY_DOWN].includes(event.code)) return;
  // Tree controls own directional keys. Letting them bubble would move the
  // unrelated file-list selection while focus visibly remains in the tree.
  event.stopPropagation();
  if (event.value !== 1 || event.modifiers !== 0 || !expandable) return;
  if ((event.code === KEY_RIGHT && !expanded) || (event.code === KEY_LEFT && expanded)) onToggle();
}

/** Inline rename field shared by the icon/list/details views. Explorer behavior:
 * the box hugs the current text (real atlas advances, recomputed on every
 * keystroke, clamped to the cell/column) on the shared text-input chrome and
 * auto-focuses on appearance so typing works without a click. */
export function RenameInput({ value, maxWidth, onChange, onCommit, onCancel }: {
  value: string;
  maxWidth: number;
  onChange: (value: string) => void;
  onCommit: () => void;
  onCancel: () => void;
}) {
  // 4px horizontal padding + 2px caret + 2px slack; never narrower than the
  // caret alone, never wider than the label cell/column it replaces.
  const width = Math.min(maxWidth, Math.max(16, Math.ceil(measureText11(value)) + 8));
  return (
    <TextInput
      width={width}
      autoFocus
      value={value}
      onInput={onChange}
      onKeyDown={(rawEvent) => {
        const key = rawEvent as { code: number; value: number };
        if (key.value !== 1) return;
        if (key.code === KEY_ENTER) onCommit();
        else if (key.code === KEY_ESC) onCancel();
      }}
    />
  );
}

interface FolderViewChrome {
  viewMode: ViewMode;
  entries: FsEntry[];
  error: string | null;
  notice: string | null;
  /** Empty-folder/search result message in the owning app's locale. */
  emptyLabel: string;
  iconLarge: (entry: FsEntry) => string;
  iconSmall: (entry: FsEntry) => string;
  entryType: (entry: FsEntry) => string;
  columns: { name: string; size: string; type: string; mtime: string };
  /** App-locale mtime cell formatter. */
  formatDate: (mtime: number) => string;
  sort: SortState;
  onSort: (column: SortColumn) => void;
  selected: string[];
  /** Names cut from the visible directory, rendered subdued until Paste. */
  cut: string[];
  renaming: string | null;
  renameDraft: string;
  /** Icon-cell content width the rename box may not exceed (list/details use
   * a wider fixed cap). */
  renameMaxWidth: number;
  onSelect: (entry: FsEntry, modifiers: number) => void;
  onOpen: (entry: FsEntry) => void;
  onEntryMenu: (entry: FsEntry, x: number, y: number) => void;
  onBlankMenu?: (x: number, y: number) => void;
  onBlankClick?: () => void;
  /** Optional group heading above the entries. */
  heading?: string;
  onRenameDraftChange: (value: string) => void;
  onRenameCommit: () => void;
  onRenameCancel: () => void;
}

/** One details header cell with a font-independent sort-direction icon. */
function HeaderCell({ className, column, label, sort, onSort }: {
  className: string;
  column: SortColumn;
  label: string;
  sort: SortState;
  onSort: (column: SortColumn) => void;
}) {
  const direction = sort.column === column ? (sort.ascending ? "sort-up" : "sort-down") : null;
  return (
    <button className={className} onClick={() => onSort(column)}>
      <span>{label}</span>
      {direction && <SystemIcon name={direction}/>}
    </button>
  );
}

interface RowProps {
  entry: FsEntry;
  selected: boolean;
  cut: boolean;
  renaming: boolean;
  renameDraft: string;
  renameMaxWidth: number;
  icon: string;
  onSelect: (entry: FsEntry, modifiers: number) => void;
  onOpen: (entry: FsEntry) => void;
  onEntryMenu: (entry: FsEntry, x: number, y: number) => void;
  onRenameDraftChange: (value: string) => void;
  onRenameCommit: () => void;
  onRenameCancel: () => void;
}

function rowCallbacks(props: RowProps) {
  return {
    onClick: (rawEvent: unknown) => {
      const event = rawEvent as LitePointerEvent;
      event.stopPropagation();
      props.onSelect(props.entry, event.modifiers);
    },
    onDoubleClick: () => props.onOpen(props.entry),
    onContextMenu: (rawEvent: unknown) => {
      const event = rawEvent as LitePointerEvent;
      event.stopPropagation();
      props.onEntryMenu(props.entry, event.x, event.y);
    },
  };
}

function RenameOrLabel({ className, name, props }: {
  className: string;
  name: string;
  props: RowProps;
}) {
  return props.renaming ? (
    <RenameInput
      value={props.renameDraft}
      maxWidth={props.renameMaxWidth}
      onChange={props.onRenameDraftChange}
      onCommit={props.onRenameCommit}
      onCancel={props.onRenameCancel}
    />
  ) : (
    <span className={className}>{name}</span>
  );
}

function IconCell(props: RowProps) {
  const className = `icon-cell${props.selected ? " icon-cell--sel" : ""}${props.cut ? " icon-cell--cut" : ""}`;
  return (
    <div className={className} {...rowCallbacks(props)}>
      <img className="icon-cell__img" src={props.icon}/>
      <RenameOrLabel className="icon-cell__label" name={props.entry.name} props={props}/>
    </div>
  );
}

function ListRow(props: RowProps) {
  const className = `list-row${props.selected ? " list-row--sel" : ""}${props.cut ? " list-row--cut" : ""}`;
  return (
    <div className={className} {...rowCallbacks(props)}>
      <img className="list-row__img" src={props.icon}/>
      <RenameOrLabel className="list-row__name" name={props.entry.name} props={props}/>
    </div>
  );
}

function DetailsRow(props: RowProps & {
  type: string;
  size: string;
  mtime: string;
}) {
  const className = `details-row${props.selected ? " details-row--sel" : ""}${props.cut ? " details-row--cut" : ""}`;
  return (
    <div className={className} {...rowCallbacks(props)}>
      <img className="details-row__img" src={props.icon}/>
      <RenameOrLabel className="details-cell-name" name={props.entry.name} props={props}/>
      <span className="details-cell-size">{props.size}</span>
      <span className="details-cell-type">{props.type}</span>
      <span className="details-cell-mtime">{props.mtime}</span>
    </div>
  );
}

/** The scrollable folder area: large icons, list, or details rows, with
 * selection, inline rename and context menus. All fs logic lives in the
 * `useBrowser` hook; this component is pure rendering. */
export function FolderView(props: FolderViewChrome) {
  const { viewMode, entries, error, notice, selected, renaming, renameDraft } = props;
  const rowProps = (entry: FsEntry): RowProps => ({
    entry,
    selected: selected.includes(entry.name),
    cut: props.cut.includes(entry.name),
    renaming: renaming === entry.name,
    renameDraft,
    renameMaxWidth: viewMode === "icons" ? props.renameMaxWidth : 280,
    icon: viewMode === "icons" ? props.iconLarge(entry) : props.iconSmall(entry),
    onSelect: props.onSelect,
    onOpen: props.onOpen,
    onEntryMenu: props.onEntryMenu,
    onRenameDraftChange: props.onRenameDraftChange,
    onRenameCommit: props.onRenameCommit,
    onRenameCancel: props.onRenameCancel,
  });
  return (
    <div
      className="folder-view"
      onClick={props.onBlankClick}
      onContextMenu={props.onBlankMenu ? (rawEvent) => {
        const event = rawEvent as unknown as { x: number; y: number };
        props.onBlankMenu!(event.x, event.y);
      } : undefined}
    >
      {error && <div className="folder-view__note">{error}</div>}
      {notice && <div className="folder-view__notice">{notice}</div>}
      {props.heading && <div className="cat-heading">{props.heading}</div>}
      {entries.length === 0 && !error && (
        <div className="folder-view__empty">{props.emptyLabel}</div>
      )}
      {entries.length > 0 && viewMode === "icons" && (
        <div className="icon-grid">
          {entries.map((entry) => <IconCell key={entry.name} {...rowProps(entry)}/>)}
        </div>
      )}
      {entries.length > 0 && viewMode === "list" && (
        <div className="list-view">
          {entries.map((entry) => <ListRow key={entry.name} {...rowProps(entry)}/>)}
        </div>
      )}
      {entries.length > 0 && viewMode === "details" && (
        <div className="details-view">
          <div className="details-header">
            <HeaderCell className="details-col-name" column="name" label={props.columns.name} sort={props.sort} onSort={props.onSort}/>
            <HeaderCell className="details-col-size" column="size" label={props.columns.size} sort={props.sort} onSort={props.onSort}/>
            <HeaderCell className="details-col-type" column="type" label={props.columns.type} sort={props.sort} onSort={props.onSort}/>
            <HeaderCell className="details-col-mtime" column="mtime" label={props.columns.mtime} sort={props.sort} onSort={props.onSort}/>
          </div>
          {entries.map((entry) => (
            <DetailsRow
              key={entry.name}
              {...rowProps(entry)}
              type={props.entryType(entry)}
              size={formatSizeFor(entry)}
              mtime={props.formatDate(entry.mtime)}
            />
          ))}
        </div>
      )}
    </div>
  );
}

/** Directories under one tree node (symlinks included, like Explorer). */
export function subdirs(path: string): FsEntry[] {
  return fsListing(path).entries.filter((entry) => entry.kind === "dir" || entry.kind === "symlink");
}

function TreeRow({ path, label, icon, depth, current, expanded, expandable, onToggle, onNavigate }: {
  path: string;
  label: string;
  icon: string;
  depth: number;
  current: boolean;
  expanded: boolean;
  expandable: boolean;
  onToggle: () => void;
  onNavigate: (path: string) => void;
}) {
  const className = `tree__row${current ? " tree__row--sel" : ""}`;
  return (
    <div
      className={className}
      style={{ paddingLeft: 4 + depth * 14 }}
    >
      {expandable ? (
        <button
          className="tree__toggle"
          aria-label={`${expanded ? "Collapse" : "Expand"} ${label}`}
          aria-expanded={expanded}
          onKeyDown={(event) => handleTreeKey(event, expanded, expandable, onToggle)}
          onClick={onToggle}
        >
          <SystemIcon name={expanded ? "chevron-down" : "chevron-right"}/>
        </button>
      ) : <span className="tree__toggle"/>}
      <button
        className="tree__destination"
        aria-current={current ? "page" : undefined}
        onKeyDown={(event) => handleTreeKey(event, expanded, expandable, onToggle)}
        onClick={() => onNavigate(path)}
      >
        <img className="tree__icon" src={icon} alt=""/>
        <span className="tree__label">{label}</span>
      </button>
    </div>
  );
}

/** Lazy-loaded directory tree. Children are listed on first expand or while
 * revealing the current path (lite:fs list is synchronous); the chevron
 * toggles expansion and clicking a row navigates. */
export function FolderTree({ roots, currentPath, revision, listDirs, onNavigate }: {
  roots: { path: string; label: string; icon: string }[];
  currentPath: string;
  /** Changes after filesystem mutations so expanded nodes do not stay stale. */
  revision: unknown;
  listDirs: (path: string) => FsEntry[];
  onNavigate: (path: string) => void;
}) {
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());
  const [children, setChildren] = useState<Map<string, FsEntry[]>>(() => new Map());
  useEffect(() => {
    if (!currentPath.startsWith("/") || currentPath === "/") return;
    const parts = currentPath.split("/").filter(Boolean);
    const parents = ["/"];
    let parent = "";
    for (const part of parts.slice(0, -1)) {
      parent += `/${part}`;
      parents.push(parent);
    }
    setExpanded((current) => {
      if (parents.every((path) => current.has(path))) return current;
      const next = new Set(current);
      for (const path of parents) next.add(path);
      return next;
    });
    setChildren((current) => {
      if (parents.every((path) => current.has(path))) return current;
      const next = new Map(current);
      for (const path of parents) {
        if (!next.has(path)) next.set(path, listDirs(path));
      }
      return next;
    });
  }, [currentPath, listDirs]);
  useEffect(() => {
    if (expanded.size === 0) return;
    setChildren(new Map(Array.from(expanded, (path) => [path, listDirs(path)])));
  }, [expanded, listDirs, revision]);
  const toggle = (path: string) => {
    setExpanded((current) => {
      const next = new Set(current);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
    if (!children.has(path)) {
      setChildren((current) => new Map(current).set(path, listDirs(path)));
    }
  };

  const renderNode = (path: string, label: string, icon: string, depth: number): React.ReactNode[] => {
    const open = expanded.has(path);
    const knownChildren = children.get(path);
    const rows: React.ReactNode[] = [
      <TreeRow
        key={path}
        path={path}
        label={label}
        icon={icon}
        depth={depth}
        current={currentPath === path}
        expanded={open}
        expandable={knownChildren === undefined || knownChildren.length > 0}
        onToggle={() => toggle(path)}
        onNavigate={onNavigate}
      />,
    ];
    if (open) {
      for (const entry of knownChildren ?? []) {
        rows.push(...renderNode(`${path === "/" ? "" : path}/${entry.name}`, entry.name, "assets/folder-16.png", depth + 1));
      }
    }
    return rows;
  };

  return (
    <div className="tree">
      {roots.map((root) => renderNode(root.path, root.label, root.icon, 0))}
    </div>
  );
}

/** Row context menu: Open/Cut/Copy/Delete/Rename (+ optional Properties)
 * operate on one entry. */
export function entryMenu(
  labels: { open: string; cut: string; copy: string; delete: string; rename: string; properties?: string },
  actions: { onOpen: () => void; onCut: () => void; onCopy: () => void; onDelete: () => void; onRename: () => void; onProperties?: () => void },
): MenuItem[] {
  const items: MenuItem[] = [
    { id: "open", label: labels.open, onSelect: actions.onOpen },
    { id: "cut", label: labels.cut, onSelect: actions.onCut },
    { id: "copy", label: labels.copy, onSelect: actions.onCopy },
    { id: "delete", label: labels.delete, onSelect: actions.onDelete },
    { id: "rename", label: labels.rename, onSelect: actions.onRename },
  ];
  if (labels.properties && actions.onProperties) {
    items.push({ id: "properties", label: labels.properties, onSelect: actions.onProperties });
  }
  return items;
}

/** Empty-area context menu: New Folder, Paste (only with a clipboard), Refresh. */
export function blankMenu(
  labels: { newFolder: string; paste: string; refresh: string },
  actions: { onNewFolder: () => void; onPaste?: () => void; onRefresh: () => void },
): MenuItem[] {
  const items: MenuItem[] = [{ id: "new", label: labels.newFolder, onSelect: actions.onNewFolder }];
  if (actions.onPaste) items.push({ id: "paste", label: labels.paste, onSelect: actions.onPaste });
  items.push({ id: "refresh", label: labels.refresh, onSelect: actions.onRefresh });
  return items;
}
