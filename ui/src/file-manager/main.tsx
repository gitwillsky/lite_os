import React, { useCallback, useEffect, useRef, useState } from "react";
import { list, read } from "lite:fs";
import type { FsEntry } from "lite:fs";

/** Joins a directory path with a child name, keeping a single leading slash. */
function joinPath(dir: string, name: string): string {
  return dir === "/" ? `/${name}` : `${dir}/${name}`;
}

/** Parent directory of an absolute path (`/` stays `/`). */
function parentPath(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const cut = trimmed.lastIndexOf("/");
  return cut <= 0 ? "/" : trimmed.slice(0, cut);
}

function formatSize(entry: FsEntry): string {
  if (entry.kind === "dir") return "";
  if (entry.kind === "symlink") return "link";
  if (entry.size < 1024) return `${entry.size} B`;
  if (entry.size < 1024 * 1024) return `${Math.round(entry.size / 1024)} KB`;
  return `${Math.round(entry.size / (1024 * 1024))} MB`;
}

function glyphFor(entry: FsEntry): string {
  if (entry.kind === "dir") return "[+]";
  if (entry.kind === "symlink") return "->";
  return "[ ]";
}

interface FileView {
  name: string;
  content: string;
  truncated: boolean;
  error?: string;
}

export default function FileManager() {
  const [path, setPath] = useState<string>("/");
  const [entries, setEntries] = useState<FsEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [fileView, setFileView] = useState<FileView | null>(null);
  const [hovered, setHovered] = useState<string | null>(null);

  // Re-list whenever the directory changes; a failed op returns a JSON error
  // object (never throws), but wrap defensively regardless.
  useEffect(() => {
    if (fileView) return;
    try {
      const result = list(path);
      if (result.error) {
        setEntries([]);
        setError(result.error);
      } else {
        const rows = (result.entries ?? []).slice().sort((a, b) => {
          if ((a.kind === "dir") !== (b.kind === "dir")) return a.kind === "dir" ? -1 : 1;
          return a.name < b.name ? -1 : 1;
        });
        setEntries(rows);
        setError(result.truncated ? "…more entries not shown" : null);
      }
    } catch {
      setEntries([]);
      setError("failed to read directory");
    }
  }, [path, fileView]);

  const goto = useCallback((next: string) => { setFileView(null); setPath(next); }, []);
  const openEntry = useCallback((entry: FsEntry) => {
    const child = joinPath(path, entry.name);
    if (entry.kind === "dir" || entry.kind === "symlink") {
      goto(child);
      return;
    }
    try {
      const result = read(child);
      setFileView({
        name: entry.name,
        content: result.content ?? "",
        truncated: Boolean(result.truncated),
        error: result.error,
      });
    } catch {
      setFileView({ name: entry.name, content: "", truncated: false, error: "IO" });
    }
  }, [path, goto]);

  // Per-name hover handlers, cached so listener ids stay stable across renders
  // (the compositor tracks hover by listener identity).
  const cache = useRef(new Map<string, { onPointerEnter: () => void; onPointerLeave: () => void }>()).current;
  const hoverHandlers = useCallback((name: string) => {
    let bundle = cache.get(name);
    if (!bundle) {
      bundle = {
        onPointerEnter: () => setHovered(name),
        onPointerLeave: () => setHovered((current) => (current === name ? null : current)),
      };
      cache.set(name, bundle);
    }
    return bundle;
  }, [cache]);

  if (fileView) {
    return (
      <div className="fm">
        <div className="fm__pathbar">
          <span className="fm__glyph" onClick={() => setFileView(null)}>{"<-"}</span>
          <span className="fm__name">{joinPath(path, fileView.name)}</span>
        </div>
        {fileView.error === "not-text"
          ? <div className="fm__note">Binary file — cannot display.</div>
          : fileView.error
            ? <div className="fm__note">Error: {fileView.error}</div>
            : <div className="fm__fileview"><span>{fileView.content}{fileView.truncated ? "\n… (truncated)" : ""}</span></div>}
      </div>
    );
  }

  return (
    <div className="fm">
      <div className="fm__pathbar">
        <span className="fm__glyph" onClick={() => path !== "/" && goto(parentPath(path))}>{".."}</span>
        <span className="fm__name">{path}</span>
      </div>
      <div className="fm__list">
        <div>
          {error && <div className="fm__note">{error}</div>}
          {entries.map((entry) => (
            <div
              key={entry.name}
              className={hovered === entry.name ? "fm__row fm__row--hover" : "fm__row"}
              {...hoverHandlers(entry.name)}
              onDoubleClick={() => openEntry(entry)}
            >
              <span className="fm__glyph">{glyphFor(entry)}</span>
              <span className="fm__name">{entry.name}</span>
              <span className="fm__size">{formatSize(entry)}</span>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
