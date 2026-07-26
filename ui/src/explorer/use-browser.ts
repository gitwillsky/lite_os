import { useCallback, useEffect, useRef, useState } from "react";
import { list, mkdir, remove, rename, copy } from "lite:fs";
import type { FsEntry } from "lite:fs";
import { baseName, freshFolderName, joinPath, parentPath } from "./model.ts";

export type ViewMode = "icons" | "list" | "details";

/** A cut/copied entry awaiting Paste; `mode` decides move vs copy. */
export interface Clipboard {
  mode: "cut" | "copy";
  path: string;
}

/** One element's cached pointer listeners. Their identities must stay stable
 * across renders because the compositor tracks hover by listener identity;
 * click handlers are passed inline at the call site since only hover depends
 * on a stable identity. */
export interface Handlers {
  onPointerEnter: () => void;
  onPointerLeave: () => void;
}

/** Pointer handlers cached by a namespaced key ("row:name", "tb:up",
 * "grp:places", …) so their identities — and thus the host listener ids the
 * compositor tracks hover by — stay stable across renders. Entry keys must be
 * cleared on navigation since the previous directory's handlers are dead. */
export function useHover() {
  const [hovered, setHovered] = useState<string | null>(null);
  const cache = useRef(new Map<string, Handlers>()).current;
  const bundle = useCallback((key: string): Handlers => {
    let handlers = cache.get(key);
    if (!handlers) {
      handlers = {
        onPointerEnter: () => setHovered(key),
        onPointerLeave: () => setHovered((current) => (current === key ? null : current)),
      };
      cache.set(key, handlers);
    }
    return handlers;
  }, [cache]);
  const cls = useCallback(
    (base: string, key: string, extra?: string) =>
      `${base}${hovered === key ? ` ${base}--hover` : ""}${extra ? ` ${extra}` : ""}`,
    [hovered],
  );
  const clear = useCallback(() => {
    cache.clear();
    setHovered(null);
  }, [cache]);
  return { hovered, bundle, cls, clear };
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
  /** Lists one location. Default: `lite:fs` directory listing, sorted
   * directories-first like Explorer. */
  listEntries?: (path: string) => Listing;
  /** Parent of a location. Default: filesystem parent (`/` is its own parent,
   * which disables Up at the root). */
  parentOf?: (path: string) => string;
  /** Navigation target for double-clicking an entry, or null to keep it
   * selected (files have no associated-program system here). */
  openTarget?: (path: string, entry: FsEntry) => string | null;
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

/** Folder-browsing state shared by file-manager and my-computer: location
 * history (Back/Forward/Up), directory listing, selection, clipboard,
 * inline rename and the fs mutations behind the context/task-pane menus. */
export function useBrowser(initialPath: string, options: BrowserOptions = {}) {
  const listEntries = options.listEntries ?? fsListing;
  const parentOf = options.parentOf ?? parentPath;
  const openTarget = options.openTarget ?? defaultOpenTarget;

  const [path, setPath] = useState<string>(initialPath);
  // Visited-path stack for real Back/Forward (Up stays "parent", distinct from
  // Back). `historyIndex` points at the current entry; navigating truncates any
  // forward tail, matching a browser/Explorer history.
  const [history, setHistory] = useState<string[]>([initialPath]);
  const [historyIndex, setHistoryIndex] = useState(0);
  const [entries, setEntries] = useState<FsEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [viewMode, setViewMode] = useState<ViewMode>("icons");
  const [selected, setSelected] = useState<string | null>(null);
  const hover = useHover();
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
    setEntries(result.entries);
    setError(result.error);
  }, [path, listEntries]);

  useEffect(() => { refresh(); }, [refresh]);

  // Clear the per-directory hover/selection state and dismiss transient popups.
  const resetView = useCallback(() => {
    hover.clear();
    setSelected(null);
    setRenaming(null);
    setAddressDraft(null);
  }, [hover]);

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

  // Double-click opens a folder (navigate in). Files have no associated-program
  // system here, so double-clicking a file only keeps it selected — matching XP,
  // where opening a file is delegated to its handler rather than the shell.
  const openEntry = useCallback((entry: FsEntry) => {
    const target = openTarget(path, entry);
    if (target !== null) {
      navigate(target);
    } else {
      setSelected(entry.name);
    }
  }, [path, navigate, openTarget]);

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
    setSelected((current) => (current === entry.name ? null : current));
  }, [path, applyResult]);

  const paste = useCallback(() => {
    if (!clipboard) return;
    const target = joinPath(path, baseName(clipboard.path));
    const result = clipboard.mode === "cut"
      ? rename(clipboard.path, target)
      : copy(clipboard.path, target);
    if (clipboard.mode === "cut" && !result.error) setClipboard(null);
    applyResult(result);
  }, [clipboard, path, applyResult]);

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
    path, entries, error, viewMode, setViewMode, selected, setSelected,
    clipboard, setClipboard, renaming, renameDraft, setRenameDraft,
    addressDraft, setAddressDraft,
    hover, refresh, navigate, back, forward, up, openEntry,
    newFolder, deleteEntry, paste, beginRename, commitRename, cancelRename,
    canBack, canForward, canUp,
  };
}

export type Browser = ReturnType<typeof useBrowser>;
