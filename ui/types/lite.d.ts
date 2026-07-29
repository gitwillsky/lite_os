// Ambient typings for the LiteOS renderer.
//
// The renderer emits standard DOM elements (div/span/img), so intrinsic element
// types come from stock `@types/react`. This file declares the LiteOS-specific
// pieces: the custom event payloads the compositor dispatches
// (`LitePointerEvent`/`LiteKeyEvent`), shared data shapes, the virtual `lite:*`
// modules, and the `globalThis.__lite*` bridge.
//
// This is an ambient script (no top-level import/export), so every type below
// is global.

/** Frozen plain-text subset of the standard Async Clipboard API. */
interface Clipboard {
  readText(): Promise<string>;
  writeText(text: string): Promise<void>;
}

interface Navigator {
  readonly clipboard: Clipboard;
}

/** Pointer payload delivered to onClick / onPointer / onContextMenu handlers. */
interface LitePointerEvent {
  type: "pointer";
  phase: "motion" | "down" | "up";
  x: number;
  y: number;
  button: number;
  buttons: number;
  serial: number;
  stopPropagation(): void;
  stopImmediatePropagation(): void;
}

/** Keyboard payload delivered to onKeyDown. */
interface LiteKeyEvent {
  type: "key";
  code: number;
  value: number;
  modifiers: number;
}

/** Pixel-mode wheel payload delivered to onWheel handlers. */
interface LiteWheelEvent {
  /** Event discriminator. */
  type: "wheel";
  /** Surface-local logical x coordinate. */
  x: number;
  /** Surface-local logical y coordinate. */
  y: number;
  /** Signed horizontal movement in CSS pixels. */
  deltaX: number;
  /** Signed vertical movement in CSS pixels. */
  deltaY: number;
  /** DOM_DELTA_PIXEL. */
  deltaMode: 0;
}

/** Logical rect carried by a foreign app surface. */
interface LiteFrame {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** One live surface in the desktop registry. */
interface LiteSurface {
  id: number;
  appId: string;
  title: string;
  icon: string;
  bounds: LiteFrame;
}

/** Desktop subscription events emitted by the compositor bridge. */
type LiteDesktopEvent =
  | { type: "opened"; surface: { id: number; appId: string } }
  | { type: "closed"; surfaceId: number }
  | { type: "activated"; surfaceId: number }
  | { type: "moved"; surfaceId: number; x: number; y: number };

/** Terminal screen snapshot (loosely typed; only the terminal app reads it). */
interface LiteScreen {
  rows: Array<Array<{ text: string; columns: number; fg: number; bg: number; bold: boolean }>>;
  cursor: { column: number; row: number; blinking?: boolean; shape?: string; visible?: boolean };
  foreground: number;
  background: number;
}

/** Frozen first-milestone HTMLMediaElement public instance returned by React refs. */
interface LiteAudioElement {
  src: string;
  currentSrc: string;
  currentTime: number;
  duration: number;
  volume: number;
  muted: boolean;
  loop: boolean;
  preload: string;
  autoplay: boolean;
  controls: boolean;
  playbackRate: 1;
  readonly paused: boolean;
  readonly ended: boolean;
  readonly seeking: boolean;
  readonly buffered: TimeRanges;
  readonly seekable: TimeRanges;
  readonly readyState: number;
  readonly networkState: number;
  readonly error: MediaError | null;
  readonly NETWORK_EMPTY: 0;
  readonly NETWORK_IDLE: 1;
  readonly NETWORK_LOADING: 2;
  readonly NETWORK_NO_SOURCE: 3;
  readonly HAVE_NOTHING: 0;
  readonly HAVE_METADATA: 1;
  readonly HAVE_CURRENT_DATA: 2;
  readonly HAVE_FUTURE_DATA: 3;
  readonly HAVE_ENOUGH_DATA: 4;
  load(): void;
  play(): Promise<void>;
  pause(): void;
  canPlayType(type: string): "" | "maybe" | "probably";
  addEventListener(type: string, listener: (event: Event) => void): void;
  removeEventListener(type: string, listener: (event: Event) => void): void;
}

// The host installs these globals before the app module evaluates.
declare var __liteReact: typeof import("react");
declare var __liteJsxRuntime: typeof import("react/jsx-runtime");
declare function __liteMount(component: (props: Record<string, never>) => import("react").ReactNode): void;
declare function __liteNative(operation: string, payload: string): string;
declare function __liteDispatch(listener: number | readonly number[], payload: unknown): void;
declare function __liteSubscribe(channel: string, callback: (event: unknown) => void): () => void;
declare function __liteTimer(id: number): void;
declare function __liteEvent(channel: string, payload: unknown): void;
declare function __liteFile(descriptor: {
  path: string;
  name: string;
  size: number;
  type: string;
  lastModified: number;
}): File;
declare function liteDesktopSubscribe(callback: (event: LiteDesktopEvent) => void): () => void;
declare function liteTerminalSubscribe(callback: (screen: LiteScreen) => void): () => void;

declare module "lite:apps" {
  export interface AppMeta {
    id: string;
    name: string;
    description: string;
    icon: string;
  }
  export const apps: () => AppMeta[];
  export const launch: (id: string) => string;
}

declare module "lite:desktop" {
  export const surfaces: () => LiteSurface[];
  export const close: (id: number) => string;
  export const focus: (id: number) => string;
  export const move: (id: number, x: number, y: number) => string;
  export const beginMove: (
    id: number,
    serial: number,
    minX: number,
    minY: number,
    maxX: number,
    maxY: number,
  ) => string;
  export const configure: (id: number, width: number, height: number) => number;
  /**
   * One fixed physical chord: an exact Shift/Ctrl/Alt/Super mask (bits 1/2/4/8)
   * plus one evdev key code, mirroring display-proto `AcceleratorChord`.
   */
  export interface AcceleratorChord {
    modifiers: number;
    code: number;
  }
  /**
   * Atomically replaces the compositor's global accelerator table (at most 16
   * chords; an empty list clears it). Desktop-only: an app session throws.
   */
  export const setAccelerators: (chords: AcceleratorChord[]) => string;
  export const shutdown: () => string;
  export const clock: () => number;
}

declare module "lite:terminal" {
  export const connect: (argv: string[]) => LiteScreen;
  export const input: (event: LiteKeyEvent) => string;
  export const paste: (text: string) => string;
}

declare module "lite:audio-system" {
  export interface MasterState {
    type: "masterstate";
    percent: number;
    muted: boolean;
  }
  export const subscribe: (callback: (state: MasterState) => void) => () => void;
  export const getState: () => string;
  export const setVolume: (percent: number) => string;
  export const setMuted: (muted: boolean) => string;
}

declare module "lite:fs" {
  export interface FsEntry {
    name: string;
    kind: "dir" | "file" | "symlink" | "other";
    size: number;
    /** Entry's own st_mtime in Unix seconds (symlinks report their own, like
     * `ls -l`); 0 when the platform cannot provide one. */
    mtime: number;
  }
  export interface FsListResult {
    path: string;
    entries?: FsEntry[];
    truncated?: boolean;
    error?: string;
  }
  export interface FsReadResult {
    path: string;
    content?: string;
    truncated?: boolean;
    error?: string;
  }
  // Mutations return the affected path and an optional error code (absent on
  // success); see the host/filesystem bounded read/write seam.
  export interface FsWriteResult {
    path: string;
    error?: string;
  }
  export const list: (path: string) => FsListResult;
  export const read: (path: string) => FsReadResult;
  export const open: (path: string) => File;
  export const mkdir: (path: string) => FsWriteResult;
  export const remove: (path: string, recursive?: boolean) => FsWriteResult;
  export const rename: (from: string, to: string) => FsWriteResult;
  export const copy: (from: string, to: string) => FsWriteResult;
}

// react-reconciler ships no bundled types and @types/react-reconciler doesn't
// match 0.33.0's HostConfig; the host-config is validated by the running
// renderer + Rust tests, so expose the factory loosely here.
declare module "react-reconciler" {
  const Reconciler: (hostConfig: unknown) => {
    createContainer: (...args: unknown[]) => unknown;
    updateContainer: (element: unknown, container: unknown, ...args: unknown[]) => unknown;
    discreteUpdates<T>(callback: () => T): T;
  };
  export default Reconciler;
}
