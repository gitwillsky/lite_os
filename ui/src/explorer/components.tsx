import React from "react";
import type { FsEntry } from "lite:fs";
import { formatSize as formatSizeFor } from "./model.ts";
import type { Handlers, ViewMode } from "./use-browser.ts";

/** One dropdown/context-menu row (shape shared with ContextMenu). */
export interface MenuItem {
  id: string;
  label: string;
  onSelect?: () => void;
}

// evdev keycodes delivered on a focused input's onKeyDown for commit/cancel.
const KEY_ESC = 1;
const KEY_ENTER = 28;

interface Chrome {
  prefix: string;
  bundle: (key: string) => Handlers;
  cls: (base: string, key: string, extra?: string) => string;
}

/** Fixed menubar row. Each label carries its dropdown rows (or null for
 * label-only chrome); the dropdown opens just under the clicked label at a
 * per-label x offset derived from the fixed stride. */
export function MenuBar({ prefix, menus, labelX, stride, top, bundle, cls, openMenu }: Chrome & {
  menus: { label: string; items: MenuItem[] | null }[];
  labelX: number;
  stride: number;
  top: number;
  openMenu: (x: number, y: number, items: MenuItem[]) => void;
}) {
  return (
    <div className={`${prefix}__menubar`}>
      {menus.map((menu, index) => (
        <span
          key={menu.label}
          className={cls(`${prefix}__menu`, `menu:${menu.label}`)}
          {...bundle(`menu:${menu.label}`)}
          onClick={() => menu.items && openMenu(labelX + index * stride, top, menu.items)}
        >
          {menu.label}
        </span>
      ))}
    </div>
  );
}

/** One standard-toolbar button; `disabled` grays the glyph and drops clicks. */
export function TbButton({ prefix, id, icon, label, disabled, bundle, cls, onClick }: Chrome & {
  id: string;
  icon: string;
  label?: string;
  disabled?: boolean;
  onClick?: () => void;
}) {
  return (
    <div
      className={cls(`${prefix}__tb`, `tb:${id}`, disabled ? `${prefix}__tb--disabled` : undefined)}
      {...bundle(`tb:${id}`)}
      onClick={() => !disabled && onClick?.()}
    >
      <img className={`${prefix}__tb-icon`} src={icon}/>
      {label && <span className={`${prefix}__tb-label`}>{label}</span>}
    </div>
  );
}

export function TbSeparator({ prefix }: { prefix: string }) {
  return <div className={`${prefix}__tb-sep`}/>;
}

/** Address row: sunken combo (icon + text) with an editable draft on focus,
 * a chevron dropdown of quick targets, and an optional Go button. */
export function AddressBar({ prefix, label, icon, text, draft, onBeginEdit, onDraftChange, onCommit, onCancel, dropItems, go, bundle, cls, openMenu }: Chrome & {
  label: string;
  icon: string;
  text: string;
  draft: string | null;
  onBeginEdit: () => void;
  onDraftChange: (value: string) => void;
  onCommit: () => void;
  onCancel: () => void;
  dropItems?: MenuItem[];
  go?: { label: string; icon: string; onClick: () => void };
  openMenu: (x: number, y: number, items: MenuItem[]) => void;
}) {
  return (
    <div className={`${prefix}__addressbar`}>
      <span className={`${prefix}__addr-label`}>{label}</span>
      <div className={`${prefix}__addr-field`} onClick={onBeginEdit}>
        <img className={`${prefix}__addr-icon`} src={icon}/>
        {draft === null ? (
          <span className={`${prefix}__addr-path`}>{text}</span>
        ) : (
          <input
            className={`${prefix}__addr-input`}
            value={draft}
            onInput={(event) => onDraftChange((event as unknown as { value: string }).value)}
            onKeyDown={(event) => {
              const key = event as unknown as { code: number; value: number };
              if (key.value === 0) return;
              if (key.code === KEY_ENTER) onCommit();
              else if (key.code === KEY_ESC) onCancel();
            }}
          />
        )}
        {dropItems && (
          <span
            className={`${prefix}__addr-drop`}
            {...bundle("addr:drop")}
            onClick={() => openMenu(8, 64, dropItems)}
          >
            <img className={`${prefix}__caret`} src="assets/caret-down.png"/>
          </span>
        )}
      </div>
      {go && (
        <div className={cls(`${prefix}__go`, "go")} {...bundle("go")} onClick={go.onClick}>
          <img className={`${prefix}__go-icon`} src={go.icon}/>
          <span>{go.label}</span>
        </div>
      )}
    </div>
  );
}

/** Inline rename field shared by the icon/list/details views. */
export function RenameInput({ prefix, value, onChange, onCommit, onCancel }: {
  prefix: string;
  value: string;
  onChange: (value: string) => void;
  onCommit: () => void;
  onCancel: () => void;
}) {
  return (
    <input
      className={`${prefix}__rename`}
      value={value}
      onInput={(event) => onChange((event as unknown as { value: string }).value)}
      onKeyDown={(event) => {
        const key = event as unknown as { code: number; value: number };
        if (key.value === 0) return;
        if (key.code === KEY_ENTER) onCommit();
        else if (key.code === KEY_ESC) onCancel();
      }}
    />
  );
}

interface FolderViewChrome extends Chrome {
  prefix: string;
  viewMode: ViewMode;
  entries: FsEntry[];
  error: string | null;
  iconLarge: (entry: FsEntry) => string;
  iconSmall: (entry: FsEntry) => string;
  entryType: (entry: FsEntry) => string;
  columns: { name: string; size: string; type: string };
  selected: string | null;
  renaming: string | null;
  renameDraft: string;
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

/** The scrollable folder area: large icons, list, or details rows, with
 * selection, inline rename and context menus. All fs logic lives in the
 * `useBrowser` hook; this component is pure rendering. */
export function FolderView(props: FolderViewChrome) {
  const { prefix, viewMode, entries, error, selected, renaming, renameDraft } = props;
  const renameProps = {
    prefix,
    value: renameDraft,
    onChange: props.onRenameDraftChange,
    onCommit: props.onRenameCommit,
    onCancel: props.onRenameCancel,
  };
  return (
    <div
      className={`${prefix}__view`}
      onClick={props.onBlankClick}
      onContextMenu={props.onBlankMenu ? (rawEvent) => {
        const event = rawEvent as unknown as { x: number; y: number };
        props.onBlankMenu!(event.x, event.y);
      } : undefined}
    >
      {error && <div className={`${prefix}__note`}>{error}</div>}
      {props.heading && <div className={`${prefix}__cat`}>{props.heading}</div>}
      {viewMode === "icons" && (
        <div className={`${prefix}__icons`}>
          {entries.map((entry) => (
            <div
              key={entry.name}
              className={props.cls(`${prefix}__icon`, `row:${entry.name}`, selected === entry.name ? `${prefix}__icon--sel` : undefined)}
              {...props.bundle(`row:${entry.name}`)}
              onClick={() => props.onSelect(entry)}
              onDoubleClick={() => props.onOpen(entry)}
              onContextMenu={(rawEvent) => {
                const event = rawEvent as unknown as { x: number; y: number };
                props.onEntryMenu(entry, event.x, event.y);
              }}
            >
              <img className={`${prefix}__icon-img`} src={props.iconLarge(entry)}/>
              {renaming === entry.name ? (
                <RenameInput {...renameProps}/>
              ) : (
                <span className={`${prefix}__icon-label`}>{entry.name}</span>
              )}
            </div>
          ))}
        </div>
      )}
      {viewMode === "list" && (
        <div className={`${prefix}__list`}>
          {entries.map((entry) => (
            <div
              key={entry.name}
              className={props.cls(`${prefix}__lrow`, `row:${entry.name}`, selected === entry.name ? `${prefix}__lrow--sel` : undefined)}
              {...props.bundle(`row:${entry.name}`)}
              onClick={() => props.onSelect(entry)}
              onDoubleClick={() => props.onOpen(entry)}
              onContextMenu={(rawEvent) => {
                const event = rawEvent as unknown as { x: number; y: number };
                props.onEntryMenu(entry, event.x, event.y);
              }}
            >
              <img className={`${prefix}__lrow-img`} src={props.iconSmall(entry)}/>
              {renaming === entry.name ? (
                <RenameInput {...renameProps}/>
              ) : (
                <span className={`${prefix}__lrow-name`}>{entry.name}</span>
              )}
            </div>
          ))}
        </div>
      )}
      {viewMode === "details" && (
        <div className={`${prefix}__details`}>
          <div className={`${prefix}__dh`}>
            <span className={`${prefix}__dh-name`}>{props.columns.name}</span>
            <span className={`${prefix}__dh-size`}>{props.columns.size}</span>
            <span className={`${prefix}__dh-type`}>{props.columns.type}</span>
          </div>
          {entries.map((entry) => (
            <div
              key={entry.name}
              className={props.cls(`${prefix}__drow`, `row:${entry.name}`, selected === entry.name ? `${prefix}__drow--sel` : undefined)}
              {...props.bundle(`row:${entry.name}`)}
              onClick={() => props.onSelect(entry)}
              onDoubleClick={() => props.onOpen(entry)}
              onContextMenu={(rawEvent) => {
                const event = rawEvent as unknown as { x: number; y: number };
                props.onEntryMenu(entry, event.x, event.y);
              }}
            >
              <img className={`${prefix}__drow-img`} src={props.iconSmall(entry)}/>
              {renaming === entry.name ? (
                <RenameInput {...renameProps}/>
              ) : (
                <span className={`${prefix}__dc-name`}>{entry.name}</span>
              )}
              <span className={`${prefix}__dc-size`}>{formatSizeFor(entry)}</span>
              <span className={`${prefix}__dc-type`}>{props.entryType(entry)}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

// Size column: directories and symlinks stay blank like Explorer; re-exported
// here so FolderView callers do not need model.ts just for the column.

/** Collapsible task-pane group box (blue header + chevron). */
export function GroupBox({ prefix, id, title, expanded, onToggle, bundle, children }: {
  prefix: string;
  id: string;
  title: string;
  expanded: boolean;
  onToggle: () => void;
  bundle: (key: string) => Handlers;
  children: React.ReactNode;
}) {
  return (
    <div className={`${prefix}__group`}>
      <div
        className={`${prefix}__group-head`}
        {...bundle(`grp:${id}`)}
        onClick={onToggle}
      >
        <span>{title}</span>
        <span className={`${prefix}__group-chev`}><img className={`${prefix}__chev`} src={expanded ? "assets/chev-up.png" : "assets/chev-down.png"}/></span>
      </div>
      {expanded && <div className={`${prefix}__group-body`}>{children}</div>}
    </div>
  );
}

/** One task-pane link; `disabled` grays it and drops clicks. */
export function TaskLink({ prefix, id, label, disabled, bundle, cls, onClick }: Chrome & {
  id: string;
  label: string;
  disabled?: boolean;
  onClick: () => void;
}) {
  return (
    <span
      className={cls(`${prefix}__task-link`, `task:${id}`, disabled ? `${prefix}__task-link--disabled` : undefined)}
      {...bundle(`task:${id}`)}
      onClick={() => { if (!disabled) onClick(); }}
    >
      {label}
    </span>
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
