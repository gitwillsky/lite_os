import { useCallback, useEffect, useMemo, useState } from "react";
import { list, mkdir, remove, rename, copy } from "lite:fs";
import type { FsEntry } from "lite:fs";
import { baseName, freshFolderName, joinPath, parentPath, typeLabel } from "./model.ts";
import type { TypeLabels } from "./model.ts";

export type ViewMode = "icons" | "list" | "details";

/** Details-view sortable columns (all backed by real FsEntry fields). */
export type SortColumn = "name" | "size" | "type" | "mtime";

export interface SortState {
  column: SortColumn;
  ascending: boolean;
}

/** A cut/copied selection awaiting Paste; `mode` decides move vs copy. */
export interface Clipboard {
  mode: "cut" | "copy";
  paths: string[];
}

/** Result of listing one location: rows plus a note banner text (or null). */
export interface Listing {
  entries: FsEntry[];
  error: string | null;
}

/** Per-app seam points. Defaults give plain filesystem browsing rooted at a
 * real directory (file-manager); My Computer overrides them to graft a
 * virtual drive-list root onto the same navigation machinery. */
export interface BrowserOptions {
  /** Wording for the Type column and type sorting (English vs zh-CN). */
  typeLabels: TypeLabels;
  /** Lists one location. Default: `lite:fs` directory listing, sorted
   * directories-first like Explorer. */
  listEntries?: (path: string) => Listing;
  /** Parent of a location. Default: filesystem parent (`/` is its own parent,
   * which disables Up at the root). */
  parentOf?: (path: string) => string;
  /** Navigation target for double-clicking an entry, or null when the entry
   * does not navigate. Default: folders navigate, files stay selected. */
  openTarget?: (path: string, entry: FsEntry) => string | null;
  /** Double-click/Enter on a file: the app decides (text viewer, …). Without
   * it a file only takes the selection, matching XP's delegate-to-handler
   * model when no handler exists. */
  onOpenFile?: (path: string, entry: FsEntry) => void;
}

/** Default {@link BrowserOptions.listEntries}: a real `lite:fs` listing. */
export function fsListing(path: string): Listing {
  try {
    const result = list(path);
    if (result.error) {
      return { entries: [], error: result.error };
    }
    const rows = (result.entries ?? []).slice().sort((a, b) => {
      if ((a.kind === "dir") !== (b.kind === "dir")) return a.kind === "dir" ? -1 : 1;
      return a.name < b.name ? -1 : 1;
    });
    return { entries: rows, error: result.truncated ? "…more entries not shown" : null };
  } catch {
    return { entries: [], error: "failed to read directory" };
  }
}

function defaultOpenTarget(path: string, entry: FsEntry): string | null {
  return entry.kind === "dir" || entry.kind === "symlink" ? joinPath(path, entry.name) : null;
}

function byName(a: FsEntry, b: FsEntry): number {
  return a.name < b.name ? -1 : a.name > b.name ? 1 : 0;
}

/** Explorer sort: folders always first (both directions), then the column;
 * name breaks ties so every column gives a stable, total order. */
function applySort(rows: FsEntry[], sort: SortState, labels: TypeLabels): FsEntry[] {
  const direction = sort.ascending ? 1 : -1;
  return rows.slice().sort((a, b) => {
    if ((a.kind === "dir") !== (b.kind === "dir")) return a.kind === "dir" ? -1 : 1;
    let result = 0;
    if (sort.column === "name") result = byName(a, b);
    else if (sort.column === "size") result = a.size - b.size || byName(a, b);
    else if (sort.column === "mtime") result = a.mtime - b.mtime || byName(a, b);
    else result = typeLabel(a, labels) < typeLabel(b, labels) ? -1 : typeLabel(a, labels) > typeLabel(b, labels) ? 1 : byName(a, b);
    return result * direction;
  });
}

/** Folder-browsing state shared by file-manager and my-computer: location
 * history (Back/Forward/Up), directory listing, multi-selection, clipboard,
 * inline rename, hidden-file filter, details sorting and the fs mutations
 * behind the context/task-pane menus. */
export function useBrowser(initialPath: string, options: BrowserOptions) {
  const typeLabels = options.typeLabels;
  const listEntries = options.listEntries ?? fsListing;
  const parentOf = options.parentOf ?? parentPath;
  const openTarget = options.openTarget ?? defaultOpenTarget;

  const [path, setPath] = useState<string>(initialPath);
  // Visited-path stack for real Back/Forward (Up stays "parent", distinct from
  // Back). `historyIndex` points at the current entry; navigating truncates any
  // forward tail, matching a browser/Explorer history.
  const [history, setHistory] = useState<string[]>([initialPath]);
  const [historyIndex, setHistoryIndex] = useState(0);
  const [rawEntries, setRawEntries] = useState<FsEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [viewMode, setViewMode] = useState<ViewMode>("icons");
  const [sort, setSort] = useState<SortState>({ column: "name", ascending: true });
  // XP hides "hidden" entries (dotfiles) until Folder Options says otherwise.
  const [showHidden, setShowHidden] = useState(false);
  // Ordered multi-selection; the last name is the focused entry (rename,
  // properties act on it; delete/cut/copy act on the whole selection).
  const [selected, setSelected] = useState<string[]>([]);
  const [clipboard, setClipboard] = useState<Clipboard | null>(null);
  // The entry name being renamed and its editable draft, or null when idle.
  const [renaming, setRenaming] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  // The address bar's editable draft when the path field is focused, else null.
  const [addressDraft, setAddressDraft] = useState<string | null>(null);

  // Re-list the current directory. Extracted from the mount effect so every
  // write (New Folder / Delete / Paste / Rename) can refresh the view.
  const refresh = useCallback(() => {
    const result = listEntries(path);
    setRawEntries(showHidden ? result.entries : result.entries.filter((entry) => !entry.name.startsWith(".")));
    setError(result.error);
  }, [path, listEntries, showHidden]);

  useEffect(() => { refresh(); }, [refresh]);

  const entries = useMemo(() => applySort(rawEntries, sort, typeLabels), [rawEntries, sort, typeLabels]);

  const toggleSort = useCallback((column: SortColumn) => {
    setSort((current) => current.column === column
      ? { column, ascending: !current.ascending }
      : { column, ascending: true });
  }, []);

  const selectOnly = useCallback((name: string) => setSelected([name]), []);
  const selectAll = useCallback(() => setSelected(rawEntries.map((entry) => entry.name)), [rawEntries]);
  const clearSelection = useCallback(() => setSelected([]), []);

  // Clear the per-directory selection/transient state on navigation.
  const resetView = useCallback(() => {
    setSelected([]);
    setRenaming(null);
    setAddressDraft(null);
  }, []);

  // Navigate to a new directory, pushing history (truncating the forward tail).
  const navigate = useCallback((next: string) => {
    resetView();
    setHistory((stack) => {
      const kept = stack.slice(0, historyIndex + 1);
      kept.push(next);
      setHistoryIndex(kept.length - 1);
      return kept;
    });
    setPath(next);
  }, [historyIndex, resetView]);

  // Jump straight to one history entry (Back/Forward dropdowns), without
  // truncating the tail — same stack semantics as browser history menus.
  const jumpTo = useCallback((index: number) => {
    if (index < 0 || index >= history.length || index === historyIndex) return;
    resetView();
    setHistoryIndex(index);
    setPath(history[index]);
  }, [history, historyIndex, resetView]);

  const back = useCallback(() => {
    if (historyIndex <= 0) return;
    const index = historyIndex - 1;
    resetView();
    setHistoryIndex(index);
    setPath(history[index]);
  }, [history, historyIndex, resetView]);

  const forward = useCallback(() => {
    if (historyIndex >= history.length - 1) return;
    const index = historyIndex + 1;
    resetView();
    setHistoryIndex(index);
    setPath(history[index]);
  }, [history, historyIndex, resetView]);

  const up = useCallback(() => navigate(parentOf(path)), [navigate, path, parentOf]);

  // Double-click/Enter opens a folder (navigate in). Files go to the app's
  // onOpenFile (text viewer); without one they only take the selection —
  // matching XP, where opening a file is delegated to its handler.
  const openEntry = useCallback((entry: FsEntry) => {
    const target = openTarget(path, entry);
    if (target !== null) {
      navigate(target);
    } else if (options.onOpenFile) {
      options.onOpenFile(path, entry);
    } else {
      setSelected([entry.name]);
    }
  }, [path, navigate, openTarget, options]);

  // Surface a native mutation's error code in the note banner, else refresh.
  const applyResult = useCallback((result: { error?: string }) => {
    if (result.error) setError(result.error);
    else refresh();
  }, [refresh]);

  const newFolder = useCallback((base: string) => {
    const taken = new Set(entries.map((entry) => entry.name));
    applyResult(mkdir(joinPath(path, freshFolderName(taken, base))));
  }, [entries, path, applyResult]);

  const deleteEntry = useCallback((entry: FsEntry) => {
    // A folder is removed with its contents (Explorer sends the whole subtree);
    // files/symlinks are unlinked. The native side gates recursion explicitly.
    applyResult(remove(joinPath(path, entry.name), entry.kind === "dir"));
    setSelected((current) => current.filter((name) => name !== entry.name));
  }, [path, applyResult]);

  const deleteSelected = useCallback((victims: FsEntry[]) => {
    for (const entry of victims) {
      applyResult(remove(joinPath(path, entry.name), entry.kind === "dir"));
    }
    setSelected((current) => current.filter((name) => !victims.some((entry) => entry.name === name)));
  }, [path, applyResult]);

  const clipboardFromSelection = useCallback((mode: Clipboard["mode"], victims: FsEntry[]) => {
    if (victims.length > 0) setClipboard({ mode, paths: victims.map((entry) => joinPath(path, entry.name)) });
  }, [path]);

  const paste = useCallback(() => {
    if (!clipboard) return;
    for (const source of clipboard.paths) {
      const target = joinPath(path, baseName(source));
      const result = clipboard.mode === "cut" ? rename(source, target) : copy(source, target);
      if (result.error) {
        setError(result.error);
        return;
      }
    }
    if (clipboard.mode === "cut") setClipboard(null);
    refresh();
  }, [clipboard, path, refresh]);

  const beginRename = useCallback((name: string) => {
    setRenaming(name);
    setRenameDraft(name);
  }, []);

  const commitRename = useCallback(() => {
    const original = renaming;
    setRenaming(null);
    if (!original || !renameDraft || renameDraft === original) return;
    applyResult(rename(joinPath(path, original), joinPath(path, renameDraft)));
  }, [renaming, renameDraft, path, applyResult]);

  const cancelRename = useCallback(() => setRenaming(null), []);

  const canBack = historyIndex > 0;
  const canForward = historyIndex < history.length - 1;
  // Up is a no-op exactly where the parent mapping is a fixed point ("/" for a
  // pure filesystem root, "" for My Computer's virtual root).
  const canUp = parentOf(path) !== path;

  return {
    path, entries, error, viewMode, setViewMode, sort, toggleSort,
    showHidden, setShowHidden,
    selected, selectOnly, selectAll, clearSelection,
    clipboard, setClipboard, renaming, renameDraft, setRenameDraft,
    addressDraft, setAddressDraft,
    history, historyIndex, jumpTo,
    refresh, navigate, back, forward, up, openEntry,
    newFolder, deleteEntry, deleteSelected, clipboardFromSelection, paste,
    beginRename, commitRename, cancelRename,
    canBack, canForward, canUp,
  };
}

export type Browser = ReturnType<typeof useBrowser>;
