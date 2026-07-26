import React, { useState } from "react";
import type { FsEntry } from "lite:fs";
import { TextInput, useHoverFlag } from "../design-system/controls.tsx";
import type { MenuItem } from "../design-system/controls.tsx";
import { formatSize as formatSizeFor, measureText11 } from "./model.ts";
import { fsListing } from "./use-browser.ts";
import type { SortColumn, SortState, ViewMode } from "./use-browser.ts";

export type { MenuItem };

// evdev keycodes delivered on a focused input's onKeyDown for commit/cancel.
const KEY_ESC = 1;
const KEY_ENTER = 28;

/** Inline rename field shared by the icon/list/details views. XP behavior:
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
        if (key.value === 0) return;
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
  iconLarge: (entry: FsEntry) => string;
  iconSmall: (entry: FsEntry) => string;
  entryType: (entry: FsEntry) => string;
  columns: { name: string; size: string; type: string; mtime: string };
  /** App-locale mtime cell formatter (zh-CN vs en-US XP date style). */
  formatDate: (mtime: number) => string;
  sort: SortState;
  onSort: (column: SortColumn) => void;
  selected: string[];
  renaming: string | null;
  renameDraft: string;
  /** Icon-cell content width the rename box may not exceed (list/details use
   * a wider fixed cap). */
  renameMaxWidth: number;
  onSelect: (entry: FsEntry) => void;
  onOpen: (entry: FsEntry) => void;
  onEntryMenu: (entry: FsEntry, x: number, y: number) => void;
  onBlankMenu?: (x: number, y: number) => void;
  onBlankClick?: () => void;
  /** Optional group heading above the entries (XP's "硬盘" category header). */
  heading?: string;
  onRenameDraftChange: (value: string) => void;
  onRenameCommit: () => void;
  onRenameCancel: () => void;
}

/** One details header cell: clickable, showing XP's ∧/∨ direction arrow on
 * the active column (the font atlas lacks ▲▼, so ASCII strokes are used). */
function HeaderCell({ className, column, label, sort, onSort }: {
  className: string;
  column: SortColumn;
  label: string;
  sort: SortState;
  onSort: (column: SortColumn) => void;
}) {
  const arrow = sort.column === column ? (sort.ascending ? " ∧" : " ∨") : "";
  return (
    <span className={className} onClick={() => onSort(column)}>
      {label}{arrow}
    </span>
  );
}

interface RowProps {
  entry: FsEntry;
  selected: boolean;
  renaming: boolean;
  renameDraft: string;
  renameMaxWidth: number;
  icon: string;
  onSelect: (entry: FsEntry) => void;
  onOpen: (entry: FsEntry) => void;
  onEntryMenu: (entry: FsEntry, x: number, y: number) => void;
  onRenameDraftChange: (value: string) => void;
  onRenameCommit: () => void;
  onRenameCancel: () => void;
}

function rowCallbacks(props: RowProps) {
  return {
    onClick: () => props.onSelect(props.entry),
    onDoubleClick: () => props.onOpen(props.entry),
    onContextMenu: (rawEvent: unknown) => {
      const event = rawEvent as { x: number; y: number };
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
  const [hovered, handlers] = useHoverFlag();
  const className = `icon-cell${hovered ? " icon-cell--hover" : ""}${props.selected ? " icon-cell--sel" : ""}`;
  return (
    <div className={className} {...handlers} {...rowCallbacks(props)}>
      <img className="icon-cell__img" src={props.icon}/>
      <RenameOrLabel className="icon-cell__label" name={props.entry.name} props={props}/>
    </div>
  );
}

function ListRow(props: RowProps) {
  const [hovered, handlers] = useHoverFlag();
  const className = `list-row${hovered ? " list-row--hover" : ""}${props.selected ? " list-row--sel" : ""}`;
  return (
    <div className={className} {...handlers} {...rowCallbacks(props)}>
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
  const [hovered, handlers] = useHoverFlag();
  const className = `details-row${hovered ? " details-row--hover" : ""}${props.selected ? " details-row--sel" : ""}`;
  return (
    <div className={className} {...handlers} {...rowCallbacks(props)}>
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
  const { viewMode, entries, error, selected, renaming, renameDraft } = props;
  const rowProps = (entry: FsEntry): RowProps => ({
    entry,
    selected: selected.includes(entry.name),
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
      {props.heading && <div className="cat-heading">{props.heading}</div>}
      {viewMode === "icons" && (
        <div className="icon-grid">
          {entries.map((entry) => <IconCell key={entry.name} {...rowProps(entry)}/>)}
        </div>
      )}
      {viewMode === "list" && (
        <div className="list-view">
          {entries.map((entry) => <ListRow key={entry.name} {...rowProps(entry)}/>)}
        </div>
      )}
      {viewMode === "details" && (
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

function TreeRow({ path, label, icon, depth, current, expanded, onToggle, onNavigate }: {
  path: string;
  label: string;
  icon: string;
  depth: number;
  current: boolean;
  expanded: boolean;
  onToggle: () => void;
  onNavigate: (path: string) => void;
}) {
  const [hovered, handlers] = useHoverFlag();
  const className = `tree__row${hovered ? " tree__row--hover" : ""}${current ? " tree__row--sel" : ""}`;
  return (
    <div
      className={className}
      {...handlers}
      style={{ paddingLeft: 4 + depth * 14 }}
      onClick={() => onNavigate(path)}
    >
      <span className="tree__toggle" onClick={onToggle}>
        {expanded ? "-" : "+"}
      </span>
      <img className="tree__icon" src={icon}/>
      <span className="tree__label">{label}</span>
    </div>
  );
}

/** XP Folders bar: a lazy-loaded directory tree. Children are listed on first
 * expand (lite:fs list is synchronous), +/- toggles expansion, clicking a row
 * navigates. The current location's row stays highlighted. */
export function FolderTree({ roots, currentPath, listDirs, onNavigate }: {
  roots: { path: string; label: string; icon: string }[];
  currentPath: string;
  listDirs: (path: string) => FsEntry[];
  onNavigate: (path: string) => void;
}) {
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());
  const [children, setChildren] = useState<Map<string, FsEntry[]>>(() => new Map());
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
    const rows: React.ReactNode[] = [
      <TreeRow
        key={path}
        path={path}
        label={label}
        icon={icon}
        depth={depth}
        current={currentPath === path}
        expanded={open}
        onToggle={() => toggle(path)}
        onNavigate={onNavigate}
      />,
    ];
    if (open) {
      for (const entry of children.get(path) ?? []) {
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
