import React, { useCallback, useRef, useState } from "react";
import { ContextMenu } from "./context-menu.tsx";

/** One dropdown/context-menu row (shape shared with ContextMenu). */
export interface MenuItem {
  id: string;
  label: string;
  onSelect?: () => void;
}

/**
 * Per-instance hover state with STABLE handler identities: the compositor
 * tracks hover by listener identity, so the two listeners are created once
 * per component instance and never change across renders. Every interactive
 * base control owns one of these instead of CSS `:hover` (unsupported by the
 * renderer).
 */
export function useHoverFlag(): [boolean, { onPointerEnter: () => void; onPointerLeave: () => void }] {
  const [hovered, setHovered] = useState(false);
  const handlers = useRef({
    onPointerEnter: () => setHovered(true),
    onPointerLeave: () => setHovered(false),
  }).current;
  return [hovered, handlers];
}

/** XP push button. `default` draws the blue default-action border; `disabled`
 * grays the label and drops clicks. */
export function Button({ label, default: isDefault, disabled, onClick }: {
  label: string;
  default?: boolean;
  disabled?: boolean;
  onClick?: () => void;
}) {
  const [hovered, handlers] = useHoverFlag();
  const className = `button${hovered && !disabled ? " button--hover" : ""}${isDefault ? " button--default" : ""}${disabled ? " button--disabled" : ""}`;
  return (
    <div className={className} {...handlers} onClick={() => !disabled && onClick?.()}>
      <span>{label}</span>
    </div>
  );
}

/** XP edit field chrome (thin themed border). Behavior props map straight
 * onto the renderer's controlled-input primitive; `autoFocus` claims focus
 * on appearance when no field owns it. */
export function TextInput({ value, width, autoFocus, placeholder, onInput, onKeyDown }: {
  value: string;
  width?: number;
  autoFocus?: boolean;
  placeholder?: string;
  onInput?: (value: string) => void;
  onKeyDown?: (event: unknown) => void;
}) {
  return (
    <input
      className="text-input"
      style={width === undefined ? undefined : { width }}
      autoFocus={autoFocus}
      placeholder={placeholder}
      value={value}
      onInput={(event) => onInput?.((event as unknown as { value: string }).value)}
      onKeyDown={onKeyDown}
    />
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
      onInput={(event) => {
        const value = Number((event as unknown as { value: string }).value);
        if (Number.isFinite(value)) onInput(value);
      }}
    />
  );
}

/** XP checkbox row: 13px sunken box with a real √ glyph when checked. */
export function CheckBox({ label, checked, disabled, onToggle }: {
  label: string;
  checked: boolean;
  disabled?: boolean;
  onToggle?: () => void;
}) {
  return (
    <div className={`checkbox${disabled ? " checkbox--disabled" : ""}`} onClick={() => !disabled && onToggle?.()}>
      <span className="checkbox__box">{checked ? "√" : ""}</span>
      <span>{label}</span>
    </div>
  );
}

/** XP radio row: circle with a filled inner dot when selected. */
export function Radio({ label, checked, disabled, onSelect }: {
  label: string;
  checked: boolean;
  disabled?: boolean;
  onSelect?: () => void;
}) {
  return (
    <div className={`radio${disabled ? " radio--disabled" : ""}`} onClick={() => !disabled && onSelect?.()}>
      <span className="radio__circle">{checked ? <span className="radio__dot"/> : null}</span>
      <span>{label}</span>
    </div>
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
    <div className="menu-bar" onClick={close}>
      {menus.map((menu, index) => (
        <MenuBarLabel
          key={menu.label}
          label={menu.label}
          onClick={() => setOpen(menu.items ? index : null)}
        />
      ))}
      {active?.items && (
        <ContextMenu x={labelX + (open ?? 0) * stride} y={20} items={active.items} onClose={close}/>
      )}
    </div>
  );
}

function MenuBarLabel({ label, onClick }: { label: string; onClick: () => void }) {
  const [hovered, handlers] = useHoverFlag();
  return (
    <span className={`menu-bar__item${hovered ? " menu-bar__item--hover" : ""}`} {...handlers} onClick={onClick}>
      {label}
    </span>
  );
}

/** Standard toolbar row: children are ToolbarButton/ToolbarSeparator. */
export function Toolbar({ children }: { children: React.ReactNode }) {
  return <div className="toolbar">{children}</div>;
}

/** One toolbar button (glyph + optional label). `disabled` grays the glyph
 * and drops clicks; `dropdown` adds XP's chevron which opens the given rows
 * at `at` (window-local, same fixed-geometry pattern as the menubar). */
export function ToolbarButton({ icon, label, disabled, dropdown, onClick }: {
  icon: string;
  label?: string;
  disabled?: boolean;
  dropdown?: { items: MenuItem[]; at: { x: number; y: number } };
  onClick?: () => void;
}) {
  const [hovered, handlers] = useHoverFlag();
  const [open, setOpen] = useState(false);
  const close = useCallback(() => setOpen(false), []);
  const className = `toolbar-button${hovered && !disabled ? " toolbar-button--hover" : ""}${disabled ? " toolbar-button--disabled" : ""}`;
  // A button with a dropdown but no action of its own opens the dropdown from
  // the body too (XP's 查看 button); the caret always opens it.
  const activate = () => {
    if (disabled) return;
    if (onClick) onClick();
    else if (dropdown) setOpen(true);
  };
  return (
    <div className={className} {...handlers} onClick={activate}>
      <img className="toolbar-button__icon" src={icon}/>
      {label && <span className="toolbar-button__label">{label}</span>}
      {dropdown && (
        <ToolbarCaret disabled={disabled} onOpen={() => setOpen(true)}/>
      )}
      {dropdown && open && (
        <ContextMenu x={dropdown.at.x} y={dropdown.at.y} items={dropdown.items} onClose={close}/>
      )}
    </div>
  );
}

function ToolbarCaret({ disabled, onOpen }: { disabled?: boolean; onOpen: () => void }) {
  const [hovered, handlers] = useHoverFlag();
  return (
    <span
      className={`toolbar-button__caret${hovered ? " toolbar-button__caret--hover" : ""}`}
      {...handlers}
      onClick={() => !disabled && onOpen()}
    >
      <img className="toolbar-button__caret-img" src="assets/caret-down.png"/>
    </span>
  );
}

/** Vertical hairline between toolbar button groups. */
export function ToolbarSeparator() {
  return <div className="toolbar-separator"/>;
}

/** Collapsible XP task-pane group box (blue header + chevron). */
export function GroupBox({ title, expanded, onToggle, children }: {
  title: string;
  expanded: boolean;
  onToggle: () => void;
  children: React.ReactNode;
}) {
  return (
    <div className="group-box">
      <div className="group-box__head" onClick={onToggle}>
        <span>{title}</span>
        <span className="group-box__chev"><img className="group-box__chev-img" src={expanded ? "assets/chev-up.png" : "assets/chev-down.png"}/></span>
      </div>
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
  const [hovered, handlers] = useHoverFlag();
  const className = `task-link${hovered && !disabled ? " task-link--hover" : ""}${disabled ? " task-link--disabled" : ""}`;
  return (
    <span className={className} {...handlers} onClick={() => { if (!disabled) onClick(); }}>
      {label}
    </span>
  );
}

/** Sectioned XP status bar. */
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
export function AddressBar({ label, icon, text, draft, onBeginEdit, onDraftChange, onCommit, onCancel, dropItems, go }: {
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
}) {
  const [open, setOpen] = useState(false);
  const close = useCallback(() => setOpen(false), []);
  const [goHovered, goHandlers] = useHoverFlag();
  return (
    <div className="address-bar">
      <span className="address-bar__label">{label}</span>
      <div className="combo-box" onClick={onBeginEdit}>
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
              const key = event as unknown as { code: number; value: number };
              if (key.value === 0) return;
              if (key.code === KEY_ENTER) onCommit();
              else if (key.code === KEY_ESC) onCancel();
            }}
          />
        )}
        {dropItems && (
          <span className="combo-box__drop" onClick={() => setOpen(true)}>
            <img className="combo-box__caret" src="assets/caret-down.png"/>
          </span>
        )}
      </div>
      {go && (
        <div className={`go-button${goHovered ? " go-button--hover" : ""}`} {...goHandlers} onClick={go.onClick}>
          <img className="go-button__icon" src={go.icon}/>
          <span>{go.label}</span>
        </div>
      )}
      {dropItems && open && (
        <ContextMenu x={8} y={64} items={dropItems} onClose={close}/>
      )}
    </div>
  );
}

// evdev keycodes delivered on a focused input's onKeyDown for commit/cancel.
const KEY_ESC = 1;
const KEY_ENTER = 28;

/** Modal XP dialog base: overlay + titled frame + content + action row.
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
    <div className="dialog-overlay">
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
