type MediaListener = (event: { type: string; target: LiteMediaElement; currentTarget: LiteMediaElement }) => void;
type LiteEventListener = ((event: Event) => void) | { handleEvent(event: Event): void };
type MediaProps = Record<string, unknown>;

interface MediaHostEvent {
  id: number;
  type: string;
  duration?: number;
  currentTime?: number;
  error?: { code: number; message: string } | null;
}

interface ShadowNode {
  id: number;
  type: string;
  props: Record<string, unknown>;
  text?: string;
  children: ShadowNode[];
}

class LiteTimeRanges {
  readonly #ranges: ReadonlyArray<readonly [number, number]>;

  constructor(ranges: ReadonlyArray<readonly [number, number]>) {
    this.#ranges = ranges;
  }

  get length() { return this.#ranges.length; }
  start(index: number) {
    return this.validIndex(index) ? this.#ranges[index][0] : this.invalidIndex();
  }
  end(index: number) {
    return this.validIndex(index) ? this.#ranges[index][1] : this.invalidIndex();
  }

  private validIndex(index: number) {
    return Number.isInteger(index) && index >= 0 && index < this.#ranges.length;
  }
  private invalidIndex(): never {
    throw new DOMException("TimeRanges index is outside its range", "IndexSizeError");
  }
}

/** Standard decode-error value exposed by `LiteMediaElement.error`. */
export class LiteMediaError {
  static readonly MEDIA_ERR_ABORTED = 1;
  static readonly MEDIA_ERR_NETWORK = 2;
  static readonly MEDIA_ERR_DECODE = 3;
  static readonly MEDIA_ERR_SRC_NOT_SUPPORTED = 4;

  readonly MEDIA_ERR_ABORTED = 1;
  readonly MEDIA_ERR_NETWORK = 2;
  readonly MEDIA_ERR_DECODE = 3;
  readonly MEDIA_ERR_SRC_NOT_SUPPORTED = 4;

  constructor(readonly code = LiteMediaError.MEDIA_ERR_DECODE, readonly message = "") {}
}

if (!("MediaError" in globalThis)) {
  Object.defineProperty(globalThis, "MediaError", {
    configurable: true,
    writable: true,
    value: LiteMediaError,
  });
}

const instances = new Map<number, LiteMediaElement>();
let mediaSubscribed = false;

function subscribeMediaEvents() {
  if (mediaSubscribed) return;
  mediaSubscribed = true;
  globalThis.__liteSubscribe("media", (payload) => {
    const event = payload as MediaHostEvent;
    instances.get(event.id)?.accept(event);
  });
}

function command(operation: string, value: unknown) {
  return globalThis.__liteNative(`media.${operation}`, JSON.stringify(value));
}

function finiteSeconds(value: number) {
  if (!Number.isFinite(value) || value < 0) {
    throw new DOMException("Media time must be a finite non-negative number", "InvalidStateError");
  }
  return value;
}

function mediaError(error: unknown) {
  if (error instanceof Error && error.message.startsWith("NotAllowedError:")) {
    return new DOMException(error.message.slice("NotAllowedError:".length).trim(), "NotAllowedError");
  }
  return error instanceof DOMException
    ? error
    : new DOMException(error instanceof Error ? error.message : String(error), "AbortError");
}

/**
 * Public HTMLMediaElement-compatible instance for LiteUI's frozen playback surface.
 *
 * File access, decode, resample, seek generations and service transport remain in
 * the process audio worker; this object owns only the observable Web state.
 */
export class LiteMediaElement {
  static readonly NETWORK_EMPTY = 0;
  static readonly NETWORK_IDLE = 1;
  static readonly NETWORK_LOADING = 2;
  static readonly NETWORK_NO_SOURCE = 3;
  static readonly HAVE_NOTHING = 0;
  static readonly HAVE_METADATA = 1;
  static readonly HAVE_CURRENT_DATA = 2;
  static readonly HAVE_FUTURE_DATA = 3;
  static readonly HAVE_ENOUGH_DATA = 4;

  readonly NETWORK_EMPTY = 0;
  readonly NETWORK_IDLE = 1;
  readonly NETWORK_LOADING = 2;
  readonly NETWORK_NO_SOURCE = 3;
  readonly HAVE_NOTHING = 0;
  readonly HAVE_METADATA = 1;
  readonly HAVE_CURRENT_DATA = 2;
  readonly HAVE_FUTURE_DATA = 3;
  readonly HAVE_ENOUGH_DATA = 4;

  readonly id: number;
  currentSrc = "";
  duration = Number.NaN;
  paused = true;
  ended = false;
  seeking = false;
  readyState = LiteMediaElement.HAVE_NOTHING;
  networkState = LiteMediaElement.NETWORK_EMPTY;
  error: LiteMediaError | null = null;
  buffered = new LiteTimeRanges([]);
  seekable = new LiteTimeRanges([]);

  #src = "";
  #currentTime = 0;
  #volume = 1;
  #muted = false;
  #loop = false;
  #preload = "metadata";
  #autoplay = false;
  #controls = false;
  #props: MediaProps = {};
  #listeners = new Map<string, Set<LiteEventListener>>();
  #playWaiters: Array<{ resolve: () => void; reject: (error: DOMException) => void }> = [];
  #playPromise: Promise<void> | null = null;
  #publish: () => void;
  #controlListeners: Record<string, number>;
  #nodeIds: number[];

  constructor(
    publish: () => void,
    addListener: (callback: (payload: unknown) => void) => number,
    nextNode: () => number,
  ) {
    subscribeMediaEvents();
    this.id = Number(command("create", {}));
    if (!Number.isSafeInteger(this.id) || this.id <= 0) throw new Error("invalid media identity");
    instances.set(this.id, this);
    this.#publish = publish;
    this.#controlListeners = {
      toggle: addListener(() => this.paused ? void this.play() : this.pause()),
      back: addListener(() => { this.currentTime = Math.max(0, this.currentTime - 10); }),
      forward: addListener(() => { this.currentTime = Math.min(this.duration || 0, this.currentTime + 10); }),
      mute: addListener(() => { this.muted = !this.muted; }),
      quieter: addListener(() => { this.volume = Math.max(0, this.volume - 0.1); }),
      louder: addListener(() => { this.volume = Math.min(1, this.volume + 0.1); }),
    };
    this.#nodeIds = Array.from({ length: 24 }, nextNode);
  }

  get src() { return this.#src; }
  set src(value: string) {
    const next = String(value);
    if (next === this.#src) return;
    this.#src = next;
    this.load();
  }

  get currentTime() { return this.#currentTime; }
  set currentTime(value: number) {
    const target = finiteSeconds(Number(value));
    command("seek", { id: this.id, time: target });
  }

  get volume() { return this.#volume; }
  set volume(value: number) {
    const next = Number(value);
    if (!Number.isFinite(next) || next < 0 || next > 1) {
      throw new DOMException("Volume must be between 0 and 1", "IndexSizeError");
    }
    if (next === this.#volume) return;
    if (!this.paused) {
      command("gain", { id: this.id, volume: next, muted: this.#muted });
    }
    this.#volume = next;
    this.dispatch("volumechange");
  }

  get muted() { return this.#muted; }
  set muted(value: boolean) {
    const next = Boolean(value);
    if (next === this.#muted) return;
    if (!this.paused) {
      command("gain", { id: this.id, volume: this.#volume, muted: next });
    }
    this.#muted = next;
    this.dispatch("volumechange");
  }

  get loop() { return this.#loop; }
  set loop(value: boolean) {
    this.#loop = Boolean(value);
    if (this.currentSrc) {
      command("loop", { id: this.id, loop: this.#loop });
    }
  }

  get preload() { return this.#preload; }
  set preload(value: string) {
    this.#preload = value === "none" || value === "auto" ? value : "metadata";
  }

  get autoplay() { return this.#autoplay; }
  set autoplay(value: boolean) { this.#autoplay = Boolean(value); }
  get controls() { return this.#controls; }
  set controls(value: boolean) { this.#controls = Boolean(value); }
  get playbackRate() { return 1; }
  set playbackRate(value: number) {
    if (Number(value) !== 1) {
      throw new DOMException("Only 1x playback is supported", "NotSupportedError");
    }
  }

  load() {
    this.rejectPlayWaiters(new DOMException("Media load replaced pending playback", "AbortError"));
    this.currentSrc = "";
    this.duration = Number.NaN;
    this.#currentTime = 0;
    this.paused = true;
    this.ended = false;
    this.seeking = false;
    this.error = null;
    this.buffered = new LiteTimeRanges([]);
    this.seekable = new LiteTimeRanges([]);
    this.readyState = LiteMediaElement.HAVE_NOTHING;
    this.networkState = this.#src ? LiteMediaElement.NETWORK_LOADING : LiteMediaElement.NETWORK_EMPTY;
    this.dispatch("emptied");
    if (!this.#src) {
      command("unload", { id: this.id });
      return;
    }
    this.dispatch("loadstart");
    command("load", { id: this.id, src: this.#src, preload: this.#preload });
  }

  play(): Promise<void> {
    if (this.#playPromise) return this.#playPromise;
    if (!this.paused) return Promise.resolve();
    if (!this.#src) {
      return Promise.reject(new DOMException("The media element has no source", "NotSupportedError"));
    }
    try {
      command("loop", { id: this.id, loop: this.#loop });
      command("gain", { id: this.id, volume: this.#volume, muted: this.#muted });
      command("play", { id: this.id, muted: this.#muted });
      this.#playPromise = new Promise((resolve, reject) => this.#playWaiters.push({
        resolve: () => { this.#playPromise = null; resolve(); },
        reject: (error) => { this.#playPromise = null; reject(error); },
      }));
      return this.#playPromise;
    } catch (error) {
      return Promise.reject(mediaError(error));
    }
  }

  pause() {
    if (this.paused && !this.#playPromise) return;
    this.rejectPlayWaiters(new DOMException("Playback was interrupted by pause()", "AbortError"));
    command("pause", { id: this.id });
  }

  canPlayType(type: string): "" | "maybe" | "probably" {
    return command("can-play-type", { type: String(type) }) as "" | "maybe" | "probably";
  }

  addEventListener(type: string, listener: LiteEventListener | null) {
    if (!listener) return;
    let listeners = this.#listeners.get(type);
    if (!listeners) this.#listeners.set(type, listeners = new Set());
    listeners.add(listener);
  }

  removeEventListener(type: string, listener: LiteEventListener | null) {
    if (!listener) return;
    this.#listeners.get(type)?.delete(listener);
  }

  dispatchEvent(event: Event) {
    for (const listener of this.#listeners.get(event.type) ?? []) {
      if (typeof listener === "function") listener.call(this, event);
      else listener.handleEvent(event);
    }
    return true;
  }

  updateProps(props: MediaProps) {
    const previousSrc = this.#src;
    const previousProps = this.#props;
    this.#props = props;
    this.#controls = Boolean(props.controls);
    this.#autoplay = Boolean(props.autoplay);
    const nextLoop = Boolean(props.loop);
    if (nextLoop !== this.#loop) this.loop = nextLoop;
    if (typeof props.preload === "string") this.preload = props.preload;
    if (typeof props.volume === "number") this.volume = props.volume;
    if (typeof props.muted === "boolean") this.muted = props.muted;
    if (typeof props.playbackRate === "number") this.playbackRate = props.playbackRate;
    if ("src" in props || "src" in previousProps) {
      const nextSrc = typeof props.src === "string" ? props.src : "";
      if (nextSrc !== previousSrc) this.src = nextSrc;
    }
  }

  accept(event: MediaHostEvent) {
    if (typeof event.duration === "number") this.duration = event.duration;
    if (typeof event.currentTime === "number") this.#currentTime = event.currentTime;
    switch (event.type) {
      case "durationchange":
        this.currentSrc = this.#src;
        this.networkState = LiteMediaElement.NETWORK_IDLE;
        this.readyState = LiteMediaElement.HAVE_METADATA;
        this.seekable = Number.isFinite(this.duration) ? new LiteTimeRanges([[0, this.duration]]) : new LiteTimeRanges([]);
        break;
      case "loadeddata":
        this.readyState = LiteMediaElement.HAVE_CURRENT_DATA;
        this.buffered = this.localBufferedRange();
        break;
      case "canplay":
        this.readyState = LiteMediaElement.HAVE_FUTURE_DATA;
        this.buffered = this.localBufferedRange();
        break;
      case "play": this.paused = false; this.ended = false; break;
      case "playing":
        this.paused = false;
        this.readyState = LiteMediaElement.HAVE_ENOUGH_DATA;
        for (const waiter of this.#playWaiters.splice(0)) waiter.resolve();
        break;
      case "pause": this.paused = true; break;
      case "seeking": this.seeking = true; this.ended = false; break;
      case "seeked": this.seeking = false; break;
      case "ended": this.ended = true; this.paused = true; break;
      case "error":
        this.buffered = new LiteTimeRanges([]);
        this.error = new LiteMediaError(
          event.error?.code ?? LiteMediaError.MEDIA_ERR_DECODE,
          event.error?.message ?? "Media decode failed",
        );
        this.paused = true;
        this.networkState = LiteMediaElement.NETWORK_NO_SOURCE;
        this.rejectPlayWaiters(new DOMException(this.error.message, "NotSupportedError"));
        break;
      case "abort":
        this.buffered = new LiteTimeRanges([]);
        this.paused = true;
        this.rejectPlayWaiters(new DOMException("Playback was aborted", "AbortError"));
        break;
    }
    this.dispatch(event.type);
    if (event.type === "loadedmetadata" && this.#autoplay) {
      void this.play().catch(() => {});
    }
  }

  destroy() {
    command("close", { id: this.id });
    instances.delete(this.id);
    this.rejectPlayWaiters(new DOMException("Media element was removed", "AbortError"));
  }

  shadowChildren(): ShadowNode[] {
    if (!this.#controls) return [];
    const ids = this.#nodeIds;
    let nextId = 0;
    const control = (text: string, listener: number): ShadowNode => {
      const id = ids[nextId++];
      const textId = ids[nextId++];
      return ({
      id,
      type: "div",
      props: {
        onClick: listener,
        style: {
          border: "2px outset #dfdfdf", background: "#d4d0c8", padding: "2px 6px",
          minWidth: 24, textAlign: "center", cursor: "pointer",
        },
      },
      children: [{ id: textId, type: "#text", props: {}, text, children: [] }],
      });
    };
    const current = formatTime(this.#currentTime);
    const duration = formatTime(this.duration);
    return [
      control(this.paused ? "Play" : "Pause", this.#controlListeners.toggle),
      control("-10", this.#controlListeners.back),
      {
        id: ids[nextId++], type: "div",
        props: { style: { flex: 1, minWidth: 100, padding: "4px", background: "#000080", color: "#ffffff" } },
        children: [{ id: ids[nextId++], type: "#text", props: {}, text: `${current} / ${duration}`, children: [] }],
      },
      control("+10", this.#controlListeners.forward),
      control(this.#muted ? "Unmute" : "Mute", this.#controlListeners.mute),
      control("Vol -", this.#controlListeners.quieter),
      {
        id: ids[nextId++], type: "div", props: { style: { width: 36, padding: "4px", textAlign: "center" } },
        children: [{ id: ids[nextId++], type: "#text", props: {}, text: `${Math.round(this.#volume * 100)}%`, children: [] }],
      },
      control("Vol +", this.#controlListeners.louder),
    ];
  }

  private dispatch(type: string) {
    const names: Record<string, string> = {
      loadstart: "onLoadStart", emptied: "onEmptied", durationchange: "onDurationChange",
      loadedmetadata: "onLoadedMetadata", loadeddata: "onLoadedData", canplay: "onCanPlay",
      play: "onPlay", playing: "onPlaying", pause: "onPause", waiting: "onWaiting",
      seeking: "onSeeking", seeked: "onSeeked", timeupdate: "onTimeUpdate", ended: "onEnded",
      volumechange: "onVolumeChange", abort: "onAbort", error: "onError",
    };
    const name = names[type];
    const listener = this.#props[name] as MediaListener | undefined;
    const event = { type, target: this, currentTarget: this };
    listener?.(event);
    this.dispatchEvent(event as unknown as Event);
    this.#publish();
  }

  private rejectPlayWaiters(error: DOMException) {
    for (const waiter of this.#playWaiters.splice(0)) waiter.reject(error);
  }

  private localBufferedRange() {
    return Number.isFinite(this.duration) && this.duration >= 0
      ? new LiteTimeRanges([[0, this.duration]])
      : new LiteTimeRanges([]);
  }
}

function formatTime(value: number) {
  if (!Number.isFinite(value) || value < 0) return "--:--";
  const seconds = Math.floor(value);
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
}
