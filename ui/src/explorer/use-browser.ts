import { useCallback, useEffect, useMemo, useState } from "react";
import { list, mkdir, remove, rename, copy } from "lite:fs";
import type { FsEntry } from "lite:fs";
import { MOD_CONTROL, MOD_SHIFT, baseName, freshCopyName, freshFolderName, joinPath, parentPath, typeLabel } from "./model.ts";
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
  /** Source directories, used to reject copying a tree into itself. */
  directories: string[];
}

/** Result of listing one location: rows plus a note banner text (or null). */
export interface Listing {
  entries: FsEntry[];
  error: string | null;
  notice: string | null;
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
   * it a file only takes the selection, matching delegate-to-handler
   * model when no handler exists. */
  onOpenFile?: (path: string, entry: FsEntry) => void;
  /** Converts native errno names into app-locale, user-facing text. */
  describeError?: (code: string) => string;
}

/** Default {@link BrowserOptions.listEntries}: a real `lite:fs` listing. */
export function fsListing(path: string): Listing {
  try {
    const result = list(path);
    if (result.error) {
      return { entries: [], error: result.error, notice: null };
    }
    const rows = (result.entries ?? []).slice().sort((a, b) => {
      if ((a.kind === "dir") !== (b.kind === "dir")) return a.kind === "dir" ? -1 : 1;
      return a.name < b.name ? -1 : 1;
    });
    return {
      entries: rows,
      error: null,
      notice: result.truncated ? "More entries exist than can be displayed" : null,
    };
  } catch {
    return { entries: [], error: "failed to read directory", notice: null };
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
  const describeError = options.describeError ?? ((code: string) => code);

  const [path, setPath] = useState<string>(initialPath);
  // Visited-path stack for real Back/Forward (Up stays "parent", distinct from
  // Back). `historyIndex` points at the current entry; navigating truncates any
  // forward tail, matching a browser/Explorer history.
  const [history, setHistory] = useState<string[]>([initialPath]);
  const [historyIndex, setHistoryIndex] = useState(0);
  const [rawEntries, setRawEntries] = useState<FsEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [viewMode, setViewMode] = useState<ViewMode>("icons");
  const [sort, setSort] = useState<SortState>({ column: "name", ascending: true });
  // Dotfiles stay hidden until Folder Options says otherwise.
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
    setError(result.error ? describeError(result.error) : null);
    setNotice(result.notice);
  }, [describeError, path, listEntries, showHidden]);

  useEffect(() => { refresh(); }, [refresh]);

  const entries = useMemo(() => applySort(rawEntries, sort, typeLabels), [rawEntries, sort, typeLabels]);
  const cutNames = useMemo(() => clipboard?.mode === "cut"
    ? clipboard.paths
      .filter((source) => parentPath(source) === path)
      .map(baseName)
    : [], [clipboard, path]);

  const toggleSort = useCallback((column: SortColumn) => {
    setSort((current) => current.column === column
      ? { column, ascending: !current.ascending }
      : { column, ascending: true });
  }, []);

  const selectOnly = useCallback((name: string) => {
    setSelected([name]);
    setRenaming((current) => current === name ? current : null);
  }, []);
  const selectWithModifiers = useCallback((ordered: FsEntry[], name: string, modifiers: number) => {
    const additive = (modifiers & MOD_CONTROL) !== 0;
    const extending = (modifiers & MOD_SHIFT) !== 0;
    setRenaming((current) => !additive && !extending && current === name ? current : null);
    setSelected((current) => {
      const clicked = ordered.findIndex((entry) => entry.name === name);
      const anchor = current.at(-1);
      const anchorIndex = anchor ? ordered.findIndex((entry) => entry.name === anchor) : -1;
      if (extending && clicked >= 0 && anchorIndex >= 0) {
        const start = Math.min(clicked, anchorIndex);
        const end = Math.max(clicked, anchorIndex);
        const range = ordered.slice(start, end + 1).map((entry) => entry.name);
        return additive ? [...new Set([...current, ...range])] : range;
      }
      if (additive) {
        return current.includes(name)
          ? current.filter((selectedName) => selectedName !== name)
          : [...current, name];
      }
      return [name];
    });
  }, []);
  const selectAll = useCallback((ordered: FsEntry[] = entries) => {
    // The owning view may pass a filtered projection (Files search/Home).
    // Selecting the unfiltered directory would make hidden rows participate
    // in Delete/Copy even though the user cannot see them.
    setSelected(ordered.map((entry) => entry.name));
    setRenaming(null);
  }, [entries]);
  const clearSelection = useCallback(() => {
    setSelected([]);
    setRenaming(null);
  }, []);
  const selectRelative = useCallback((ordered: FsEntry[], delta: -1 | 1) => {
    if (ordered.length === 0) return;
    setRenaming(null);
    setSelected((current) => {
      const focused = current.at(-1);
      const index = focused ? ordered.findIndex((entry) => entry.name === focused) : -1;
      if (index < 0) return [ordered[delta > 0 ? 0 : ordered.length - 1].name];
      const next = Math.max(0, Math.min(ordered.length - 1, index + delta));
      return [ordered[next].name];
    });
  }, []);

  // Clear the per-directory selection/transient state on navigation.
  const resetView = useCallback(() => {
    setSelected([]);
    setRenaming(null);
    setAddressDraft(null);
  }, []);

  // Navigate to a new directory, pushing history (truncating the forward tail).
  const navigate = useCallback((next: string) => {
    resetView();
    if (next === path) {
      refresh();
      return;
    }
    setHistory((stack) => {
      const kept = stack.slice(0, historyIndex + 1);
      kept.push(next);
      setHistoryIndex(kept.length - 1);
      return kept;
    });
    setPath(next);
  }, [path, historyIndex, resetView, refresh]);

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
  // Opening a file is delegated to its handler.
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
    if (result.error) {
      setError(describeError(result.error));
      setNotice(null);
    } else refresh();
  }, [describeError, refresh]);

  const newFolder = useCallback((base: string) => {
    const taken = new Set(entries.map((entry) => entry.name));
    const name = freshFolderName(taken, base);
    const result = mkdir(joinPath(path, name));
    if (result.error) {
      setError(describeError(result.error));
      setNotice(null);
      return;
    }
    refresh();
    setSelected([name]);
    setRenaming(name);
    setRenameDraft(name);
  }, [describeError, entries, path, refresh]);

  const deleteEntry = useCallback((entry: FsEntry) => {
    // A folder is removed with its contents (Explorer sends the whole subtree);
    // files/symlinks are unlinked. The native side gates recursion explicitly.
    const result = remove(joinPath(path, entry.name), entry.kind === "dir");
    applyResult(result);
    if (!result.error) {
      setSelected((current) => current.filter((name) => name !== entry.name));
    }
  }, [path, applyResult]);

  const deleteSelected = useCallback((victims: FsEntry[]) => {
    const removed = new Set<string>();
    let failure: string | undefined;
    for (const entry of victims) {
      const result = remove(joinPath(path, entry.name), entry.kind === "dir");
      if (result.error) {
        failure = result.error;
        break;
      }
      removed.add(entry.name);
    }
    if (removed.size > 0) refresh();
    if (failure) {
      setError(describeError(failure));
      setNotice(null);
    }
    setSelected((current) => current.filter((name) => !removed.has(name)));
  }, [describeError, path, refresh]);

  const clipboardFromSelection = useCallback((mode: Clipboard["mode"], victims: FsEntry[]) => {
    if (victims.length > 0) {
      const paths = victims.map((entry) => joinPath(path, entry.name));
      setClipboard({
        mode,
        paths,
        directories: victims.flatMap((entry, index) => entry.kind === "dir" ? [paths[index]] : []),
      });
    }
  }, [path]);

  const paste = useCallback(() => {
    if (!clipboard) return;
    let completed = 0;
    let failure: string | undefined;
    const taken = new Set(entries.map((entry) => entry.name));
    for (const source of clipboard.paths) {
      if (clipboard.directories.includes(source)
        && (path === source || path.startsWith(`${source}/`))) {
        failure = "EINVAL";
        break;
      }
      const sourceName = baseName(source);
      const targetName = clipboard.mode === "copy"
        ? freshCopyName(taken, sourceName, !clipboard.directories.includes(source))
        : sourceName;
      const target = joinPath(path, targetName);
      // Moving onto an existing sibling would let POSIX rename replace a file.
      // Pasting a cut item back into its source folder is only a no-op.
      if (clipboard.mode === "cut" && source !== target && taken.has(targetName)) {
        failure = "EEXIST";
        break;
      }
      const result = clipboard.mode === "cut" ? rename(source, target) : copy(source, target);
      if (result.error) {
        failure = result.error;
        break;
      }
      taken.add(targetName);
      completed += 1;
    }
    if (clipboard.mode === "cut") {
      const remaining = clipboard.paths.slice(completed);
      setClipboard(remaining.length === 0 ? null : {
        mode: "cut",
        paths: remaining,
        directories: clipboard.directories.filter((source) => remaining.includes(source)),
      });
    }
    refresh();
    if (failure) {
      setError(describeError(failure));
      setNotice(null);
    }
  }, [clipboard, describeError, entries, path, refresh]);

  const beginRename = useCallback((name: string) => {
    setRenaming(name);
    setRenameDraft(name);
  }, []);

  const commitRename = useCallback(() => {
    const original = renaming;
    if (!original) return;
    if (!renameDraft || renameDraft === original) {
      setRenaming(null);
      return;
    }
    if (renameDraft.includes("/") || renameDraft === "." || renameDraft === "..") {
      setError(describeError("EINVAL"));
      setNotice(null);
      return;
    }
    if (entries.some((entry) => entry.name === renameDraft)) {
      setError(describeError("EEXIST"));
      setNotice(null);
      return;
    }
    const result = rename(joinPath(path, original), joinPath(path, renameDraft));
    if (result.error) {
      setError(describeError(result.error));
      setNotice(null);
      return;
    }
    setRenaming(null);
    setSelected([renameDraft]);
    refresh();
  }, [describeError, entries, renaming, renameDraft, path, refresh]);

  const cancelRename = useCallback(() => setRenaming(null), []);

  const canBack = historyIndex > 0;
  const canForward = historyIndex < history.length - 1;
  // Up is a no-op exactly where the parent mapping is a fixed point ("/" for a
  // pure filesystem root, "" for My Computer's virtual root).
  const canUp = parentOf(path) !== path;

  return {
    path, entries, error, notice, viewMode, setViewMode, sort, toggleSort,
    showHidden, setShowHidden,
    selected, selectOnly, selectWithModifiers, selectAll, clearSelection, selectRelative,
    clipboard, setClipboard, cutNames, renaming, renameDraft, setRenameDraft,
    addressDraft, setAddressDraft,
    history, historyIndex, jumpTo,
    refresh, navigate, back, forward, up, openEntry,
    newFolder, deleteEntry, deleteSelected, clipboardFromSelection, paste,
    beginRename, commitRename, cancelRename,
    canBack, canForward, canUp,
  };
}

export type Browser = ReturnType<typeof useBrowser>;
