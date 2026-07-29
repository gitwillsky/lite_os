import React from "react";
import { read } from "lite:fs";
import type { FsEntry } from "lite:fs";
import { Button, CheckBox, Dialog, Radio } from "../design-system/controls.tsx";
import { joinPath } from "./model.ts";
import type { ViewMode } from "./use-browser.ts";

/** One open text viewer: title plus the (possibly capped) file content. */
export interface FileView {
  title: string;
  text: string;
  truncated: boolean;
}

/** Reads one file for the text viewer. `lite:fs read` already enforces the
 * 64 KiB cap and classifies non-UTF-8 content as "not-text", so the caller
 * only maps the outcome to viewer vs cannot-open dialog. */
export function readTextFile(path: string, entry: FsEntry): { view: FileView } | { cannotOpen: true } | { error: string } {
  const result = read(joinPath(path, entry.name));
  if (result.error === "not-text") return { cannotOpen: true };
  if (result.error) return { error: result.error };
  return {
    view: {
      title: entry.name,
      text: result.content ?? "",
      truncated: result.truncated ?? false,
    },
  };
}

/** Shared read-only viewer for text-readable files. */
export function TextViewer({ view, onClose, closeLabel, truncatedLabel }: {
  view: FileView;
  onClose: () => void;
  closeLabel: string;
  truncatedLabel: string;
}) {
  return (
    <Dialog
      title={view.title}
      wide
      onClose={onClose}
      actions={<Button label={closeLabel} default onClick={onClose}/>}
    >
      <div className="text-viewer">
        <span className="text-viewer__content">{view.text}</span>
      </div>
      {view.truncated && <div className="dialog__note"><span>{truncatedLabel}</span></div>}
    </Dialog>
  );
}

/** Unknown-type dialog: real shell
 * behavior for files with no associated handler, not silence. */
export function CannotOpenDialog({ name, message, onClose, closeLabel }: {
  name: string;
  message: (name: string) => string;
  onClose: () => void;
  closeLabel: string;
}) {
  return (
    <Dialog title={name} onClose={onClose}>
      <div className="dialog__note"><span>{message(name)}</span></div>
    </Dialog>
  );
}

/** Read-only properties dialog (label/value rows) on the shared Dialog base;
 * used by explorer apps for 属性 and 系统信息. */
export function PropertiesDialog({ title, rows, onClose, closeLabel }: {
  title: string;
  rows: [string, string][];
  onClose: () => void;
  closeLabel: string;
}) {
  return (
    <Dialog title={title} onClose={onClose}>
      {rows.map(([label, value]) => (
        <div key={label} className="properties-row">
          <span className="properties-row__label">{label}</span>
          <span className="properties-row__value">{value}</span>
        </div>
      ))}
      <Button label={closeLabel} default onClick={onClose}/>
    </Dialog>
  );
}

/** 工具 → 文件夹选项: every row acts on real live state (hidden-file filter,
 * current view mode) — session-scoped since lite:fs has no write API, and the
 * rows say what they do instead of pretending to persist. */
export function FolderOptionsDialog({ showHidden, onToggleHidden, viewMode, views, onPickView, onClose, labels }: {
  showHidden: boolean;
  onToggleHidden: () => void;
  viewMode: ViewMode;
  views: { mode: ViewMode; label: string }[];
  onPickView: (mode: ViewMode) => void;
  onClose: () => void;
  labels: { hiddenFiles: string; view: string; close: string };
}) {
  return (
    <Dialog
      title={labels.view}
      onClose={onClose}
      actions={<Button label={labels.close} default onClick={onClose}/>}
    >
      <CheckBox label={labels.hiddenFiles} checked={showHidden} onToggle={onToggleHidden}/>
      {views.map((view) => (
        <Radio
          key={view.mode}
          label={view.label}
          checked={viewMode === view.mode}
          onSelect={() => onPickView(view.mode)}
        />
      ))}
    </Dialog>
  );
}
