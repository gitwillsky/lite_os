import React, { useCallback, useState } from "react";
import { ContextMenu } from "./context-menu.tsx";
import { SYSTEM_ICON_GLYPHS } from "./system-icons.generated.ts";
import type { SystemIconName } from "./system-icons.generated.ts";

const KEY_ESC = 1;

export type { SystemIconName } from "./system-icons.generated.ts";

/** Shared typed system icon backed by the checked, self-hosted PUA font. */
export function SystemIcon({ name, className = "" }: { name: SystemIconName; className?: string }) {
  return (
    <span className={`system-icon system-icon--${name}${className ? ` ${className}` : ""}`} aria-hidden="true">
      {SYSTEM_ICON_GLYPHS[name]}
    </span>
  );
}

/** One dropdown/context-menu row (shape shared with ContextMenu). */
export interface MenuItem {
  id: string;
  label: string;
  onSelect?: () => void;
  disabled?: boolean;
  separator?: boolean;
}

/** Shared semantic push button. Visual states are standard CSS pseudo-classes. */
export function Button({ label, default: isDefault, danger, disabled, onClick }: {
  label: string;
  default?: boolean;
  danger?: boolean;
  disabled?: boolean;
  onClick?: () => void;
}) {
  return (
    <button className={`button${isDefault ? " button--default" : ""}${danger ? " button--danger" : ""}`} disabled={disabled} onClick={onClick}>
      <span className="control-label">{label}</span>
    </button>
  );
}

/** Shared controlled text field. Behavior props map straight
 * onto the renderer's controlled-input primitive; `autoFocus` claims focus
 * on appearance when no field owns it. */
export function TextInput({ value, width, className, autoFocus, placeholder, onInput, onKeyDown }: {
  value: string;
  width?: number;
  /** App-specific layout hook; control visuals remain owned by `.text-input`. */
  className?: string;
  autoFocus?: boolean;
  placeholder?: string;
  onInput?: (value: string) => void;
  onKeyDown?: (event: unknown) => void;
}) {
  return (
    <input
      className={`text-input${className ? ` ${className}` : ""}`}
      style={width === undefined ? undefined : { width }}
      autoFocus={autoFocus}
      placeholder={placeholder}
      value={value}
      onInput={(event) => onInput?.((event as unknown as { value: string }).value)}
      onKeyDown={(event) => {
        (event as unknown as LiteKeyEvent).stopPropagation();
        onKeyDown?.(event);
      }}
    />
  );
}

/** Shared search field used by system and application toolbars. */
export function SearchField({ value, placeholder, onInput, onEscape }: {
  value: string;
  placeholder: string;
  onInput: (value: string) => void;
  /** Clears the owning search before Escape falls through to app-level actions. */
  onEscape?: () => void;
}) {
  return (
    <div className="search-field">
      <SystemIcon name="search" className="search-glyph"/>
      <input
        aria-label={placeholder}
        value={value}
        placeholder={placeholder}
        onInput={(event) => onInput((event as unknown as { value: string }).value)}
        onKeyDown={(rawEvent) => {
          const event = rawEvent as unknown as LiteKeyEvent;
          if (event.code === KEY_ESC && value && event.value === 1 && onEscape) {
            event.stopPropagation();
            onEscape();
          } else if (event.code !== KEY_ESC) {
            event.stopPropagation();
          }
        }}
      />
    </div>
  );
}

/** Shared vertical navigation surface. */
export function Sidebar({ children, className }: { children: React.ReactNode; className?: string }) {
  return <div className={`sidebar${className ? ` ${className}` : ""}`}>{children}</div>;
}

/** One semantic sidebar destination. */
export function SidebarItem({ label, icon, active, onClick }: {
  label: string;
  icon: string;
  active?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      className={`sidebar-item${active ? " sidebar-item--active" : ""}`}
      aria-current={active ? "page" : undefined}
      onClick={onClick}
    >
      <img className="sidebar-item__icon" src={icon} alt=""/>
      <span className="control-label">{label}</span>
    </button>
  );
}

/** Shared grid/list selector used by explorer-style applications. */
export function ViewSwitch({ mode, onChange }: {
  mode: "icons" | "details";
  onChange: (mode: "icons" | "details") => void;
}) {
  return (
    <div className="view-switch">
      <button
        className={`view-switch__button${mode === "icons" ? " view-switch__button--active" : ""}`}
        aria-label="Grid view"
        aria-pressed={mode === "icons"}
        onClick={() => onChange("icons")}
      >
        <span className="view-switch__grid"><span/><span/><span/><span/></span>
      </button>
      <button
        className={`view-switch__button${mode === "details" ? " view-switch__button--active" : ""}`}
        aria-label="List view"
        aria-pressed={mode === "details"}
        onClick={() => onChange("details")}
      >
        <span className="view-switch__list"><span/><span/><span/></span>
      </button>
    </div>
  );
}

/** Standard controlled horizontal range using LiteUI's native range default
 * actions: pointer drag and arrow keys both emit a string-valued `onInput`. */
export function RangeInput({ value, min, max, step, disabled, className, onInput }: {
  value: number;
  min: number;
  max: number;
  step: number;
  disabled?: boolean;
  className?: string;
  onInput: (value: number) => void;
}) {
  return (
    <input
      className={`range-input${disabled ? " range-input--disabled" : ""}${className ? ` ${className}` : ""}`}
      type="range"
      min={min}
      max={max}
      step={step}
      value={value}
      disabled={disabled}
      onKeyDown={(rawEvent) => {
        const event = rawEvent as unknown as LiteKeyEvent;
        if (event.code !== KEY_ESC) event.stopPropagation();
      }}
      onInput={(event) => {
        const value = Number((event as unknown as { value: string }).value);
        if (Number.isFinite(value)) onInput(value);
      }}
    />
  );
}

/** Shared semantic checkbox row. */
export function CheckBox({ label, checked, disabled, onToggle }: {
  label: string;
  checked: boolean;
  disabled?: boolean;
  onToggle?: () => void;
}) {
  return (
    <button className="checkbox" aria-pressed={checked} disabled={disabled} onClick={onToggle}>
      <span className="checkbox__box">{checked ? <SystemIcon name="check"/> : null}</span>
      <span className="control-label">{label}</span>
    </button>
  );
}

/** Shared semantic radio row. */
export function Radio({ label, checked, disabled, onSelect }: {
  label: string;
  checked: boolean;
  disabled?: boolean;
  onSelect?: () => void;
}) {
  return (
    <button className="radio" aria-pressed={checked} disabled={disabled} onClick={onSelect}>
      <span className="radio__circle">{checked ? <span className="radio__dot"/> : null}</span>
      <span className="control-label">{label}</span>
    </button>
  );
}

/** Menu-bar row. Each label carries its dropdown rows (or null for
 * label-only chrome); the bar owns the open dropdown and its ContextMenu.
 * `labelX`/`stride` give the fixed per-label x offset (CJK labels need a
 * wider stride than English ones). */
export function MenuBar({ menus, labelX, stride }: {
  menus: { label: string; items: MenuItem[] | null }[];
  labelX: number;
  stride: number;
}) {
  const [open, setOpen] = useState<number | null>(null);
  const close = useCallback(() => setOpen(null), []);
  const active = open === null ? null : menus[open];
  return (
    <div
      className="menu-bar"
      onClick={close}
      onKeyDown={open !== null ? (rawEvent) => {
        const event = rawEvent as unknown as LiteKeyEvent;
        event.stopPropagation();
        if (event.code === KEY_ESC && event.value === 1) close();
      } : undefined}
    >
      {menus.map((menu, index) => (
        <MenuBarLabel
          key={menu.label}
          label={menu.label}
          active={open === index}
          disabled={!menu.items}
          onClick={() => setOpen((current) => menu.items && current !== index ? index : null)}
        />
      ))}
      {active?.items && (
        <ContextMenu x={labelX + (open ?? 0) * stride} y={30} items={active.items} onClose={close}/>
      )}
    </div>
  );
}

function MenuBarLabel({ label, active, disabled, onClick }: {
  label: string;
  active: boolean;
  disabled: boolean;
  onClick: () => void;
}) {
  return (
    <button
      className={`menu-bar__item${active ? " menu-bar__item--active" : ""}`}
      disabled={disabled}
      onClick={(rawEvent) => {
        (rawEvent as unknown as LitePointerEvent).stopPropagation();
        onClick();
      }}
    >
      <span className="control-label">{label}</span>
    </button>
  );
}

/** Standard toolbar row: children are ToolbarButton/ToolbarSeparator. */
export function Toolbar({ children }: { children: React.ReactNode }) {
  return <div className="toolbar">{children}</div>;
}

/** One toolbar button (glyph + optional label). `disabled` grays the glyph
 * and drops clicks; `dropdown` adds a chevron which opens the given rows
 * at `at` (window-local, same fixed-geometry pattern as the menubar). */
export function ToolbarButton({ icon, label, disabled, dropdown, onClick }: {
  icon: string;
  label?: string;
  disabled?: boolean;
  dropdown?: { items: MenuItem[]; at: { x: number; y: number } };
  onClick?: () => void;
}) {
  const [open, setOpen] = useState(false);
  const close = useCallback(() => setOpen(false), []);
  const activate = () => {
    if (onClick) {
      close();
      onClick();
    } else if (dropdown) setOpen((current) => !current);
  };
  return (
    <div
      className="toolbar-button-group"
      onKeyDown={open ? (rawEvent) => {
        const event = rawEvent as unknown as LiteKeyEvent;
        event.stopPropagation();
        if (event.code === KEY_ESC && event.value === 1) close();
      } : undefined}
    >
      <button className="toolbar-button" disabled={disabled} onClick={activate}>
        <img className="toolbar-button__icon" src={icon}/>
        {label && <span className="toolbar-button__label control-label">{label}</span>}
      </button>
      {dropdown && <ToolbarCaret disabled={disabled} onOpen={() => setOpen((current) => !current)}/>}
      {dropdown && open && (
        <ContextMenu x={dropdown.at.x} y={dropdown.at.y} items={dropdown.items} onClose={close}/>
      )}
    </div>
  );
}

function ToolbarCaret({ disabled, onOpen }: { disabled?: boolean; onOpen: () => void }) {
  return (
    <button className="toolbar-button__caret" disabled={disabled} onClick={onOpen}>
      <img className="toolbar-button__caret-img" src="assets/caret-down.png"/>
    </button>
  );
}

/** Vertical hairline between toolbar button groups. */
export function ToolbarSeparator() {
  return <div className="toolbar-separator"/>;
}

/** Shared collapsible information group. */
export function GroupBox({ title, expanded, onToggle, children }: {
  title: string;
  expanded: boolean;
  onToggle: () => void;
  children: React.ReactNode;
}) {
  return (
    <div className="group-box">
      <button className="group-box__head" aria-expanded={expanded} onClick={onToggle}>
        <span className="control-label">{title}</span>
        <span className="group-box__chev"><img className="group-box__chev-img" src={expanded ? "assets/chev-up.png" : "assets/chev-down.png"}/></span>
      </button>
      {expanded && <div className="group-box__body">{children}</div>}
    </div>
  );
}

/** One task-pane link; `disabled` grays it and drops clicks. */
export function TaskLink({ label, disabled, onClick }: {
  label: string;
  disabled?: boolean;
  onClick: () => void;
}) {
  return (
    <button className="task-link" disabled={disabled} onClick={onClick}>
      <span className="control-label">{label}</span>
    </button>
  );
}

/** Shared sectioned status bar. */
export function StatusBar({ children }: { children: React.ReactNode }) {
  return <div className="status-bar">{children}</div>;
}

/** One sunken status-bar section, with an optional leading icon. */
export function StatusBarCell({ icon, text }: { icon?: string; text: string }) {
  return (
    <span className="status-bar__cell">
      {icon && <img className="status-bar__icon" src={icon}/>}
      <span>{text}</span>
    </span>
  );
}

/** Address combo box: sunken field (icon + text or edit input) with a chevron
 * dropdown of quick targets and an optional Go button. The bar owns the
 * dropdown; `draft !== null` switches the display text for the edit field. */
export function AddressBar({ label, icon, text, draft, onBeginEdit, onDraftChange, onCommit, onCancel, dropItems, dropAt, go }: {
  label: string;
  icon: string;
  text: string;
  draft: string | null;
  onBeginEdit: () => void;
  onDraftChange: (value: string) => void;
  onCommit: () => void;
  onCancel: () => void;
  dropItems?: MenuItem[];
  /** Viewport-local placement for the optional dropdown menu. */
  dropAt?: { x: number; y: number };
  go?: { label: string; icon: string; onClick: () => void };
}) {
  const [open, setOpen] = useState(false);
  const close = useCallback(() => setOpen(false), []);
  return (
    <div
      className="address-bar"
      onKeyDown={open ? (rawEvent) => {
        const event = rawEvent as unknown as LiteKeyEvent;
        event.stopPropagation();
        if (event.code === KEY_ESC && event.value === 1) close();
      } : undefined}
    >
      <span className="address-bar__label">{label}</span>
      <div
        className="combo-box"
        onClick={draft === null ? () => {
          close();
          onBeginEdit();
        } : undefined}
      >
        <img className="combo-box__icon" src={icon}/>
        {draft === null ? (
          <span className="combo-box__text">{text}</span>
        ) : (
          <input
            className="combo-box__input"
            autoFocus
            value={draft}
            onInput={(event) => onDraftChange((event as unknown as { value: string }).value)}
            onKeyDown={(event) => {
              const key = event as unknown as LiteKeyEvent;
              key.stopPropagation();
              if (key.value === 0) return;
              if (key.code === KEY_ENTER) onCommit();
              else if (key.code === KEY_ESC) onCancel();
            }}
          />
        )}
        {dropItems && (
          <button
            className="combo-box__drop"
            aria-label="Show address history"
            onClick={(rawEvent) => {
              (rawEvent as unknown as LitePointerEvent).stopPropagation();
              setOpen((current) => !current);
            }}
          >
            <img className="combo-box__caret" src="assets/caret-down.png"/>
          </button>
        )}
      </div>
      {go && (
        <button className="go-button" onClick={go.onClick}>
          <img className="go-button__icon" src={go.icon}/>
          <span className="control-label">{go.label}</span>
        </button>
      )}
      {dropItems && open && (
        <ContextMenu x={dropAt?.x ?? 8} y={dropAt?.y ?? 64} items={dropItems} onClose={close}/>
      )}
    </div>
  );
}

// evdev keycodes delivered on a focused input's onKeyDown for commit/cancel.
const KEY_ENTER = 28;

/** Shared modal dialog base: overlay + titled frame + content + action row.
 * Every app modal (properties, options, viewer, cannot-open) builds on this;
 * `actions` defaults to a single default OK button closing the dialog. */
export function Dialog({ title, wide, onClose, actions, children }: {
  title: string;
  wide?: boolean;
  onClose: () => void;
  actions?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <div
      className="dialog-overlay"
      data-lite-focus-scope={true}
      onPointerDown={(rawEvent) => {
        // LiteUI only creates pointer hit regions for interactive nodes. The
        // handler makes the visual scrim a real modal barrier; without it a
        // click outside the dialog can target a file-row button underneath.
        (rawEvent as unknown as LitePointerEvent).stopPropagation();
      }}
    >
      <div className={`dialog${wide ? " dialog--wide" : ""}`}>
        <div className="dialog__title"><span>{title}</span></div>
        <div className="dialog__body">{children}</div>
        <div className="dialog__actions">
          {actions ?? <Button label="OK" default onClick={onClose}/>}
        </div>
      </div>
    </div>
  );
}
