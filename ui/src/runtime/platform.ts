// Minimal platform polyfills backing the React runtime on the LiteOS host:
// timers and microtasks route through the native bridge, and a channel
// registry powers `__liteSubscribe`/`__liteEvent`. These assign host globals
// whose lib signatures don't match the compositor ABI, so the shim object is
// installed through an `any` view of globalThis by design.
const timers = new Map<number, () => void>();
const channels = new Map<string, Set<(payload: unknown) => void>>();
const clipboardReads = new Map<number, (text: string) => void>();
let nextTimer = 1;

interface NativeFileDescriptor {
  path: string;
  name: string;
  size: number;
  type: string;
  lastModified: number;
}

class LiteBlob {
  readonly size: number;
  readonly type: string;
  readonly #nativePath: string;
  readonly #offset: number;

  constructor(nativePath: string, offset: number, size: number, type: string) {
    this.#nativePath = nativePath;
    this.#offset = offset;
    this.size = size;
    this.type = type.toLowerCase();
  }

  slice(start = 0, end = this.size, contentType = ""): Blob {
    const relativeStart = start < 0 ? Math.max(this.size + start, 0) : Math.min(start, this.size);
    const relativeEnd = end < 0 ? Math.max(this.size + end, 0) : Math.min(end, this.size);
    const length = Math.max(relativeEnd - relativeStart, 0);
    return new LiteBlob(
      this.#nativePath,
      this.#offset + relativeStart,
      length,
      /^[\x20-\x7e]*$/.test(contentType) ? contentType : "",
    ) as unknown as Blob;
  }
  arrayBuffer(): Promise<ArrayBuffer> {
    return this.bytes().then((bytes) => bytes.buffer.slice(
      bytes.byteOffset,
      bytes.byteOffset + bytes.byteLength,
    ) as ArrayBuffer);
  }
  bytes(): Promise<Uint8Array> {
    return Promise.resolve().then(() => new Uint8Array(JSON.parse(globalThis.__liteNative(
      "fs.read-range",
      JSON.stringify({ path: this.#nativePath, offset: this.#offset, length: this.size }),
    )) as number[]));
  }
  text(): Promise<string> {
    return this.bytes().then(decodeUtf8);
  }
  stream(): ReadableStream<Uint8Array> {
    return new LiteReadableStream(() => this.bytes()) as unknown as ReadableStream<Uint8Array>;
  }

  descriptor() {
    return { path: this.#nativePath, offset: this.#offset, length: this.size };
  }
}

class LiteFile extends LiteBlob {
  readonly name: string;
  readonly lastModified: number;

  constructor(descriptor: NativeFileDescriptor) {
    super(descriptor.path, 0, descriptor.size, descriptor.type);
    this.name = descriptor.name;
    this.lastModified = descriptor.lastModified;
  }

}

const liveObjectUrls = new Set<string>();

class LiteReadableStream {
  locked = false;
  #read: () => Promise<Uint8Array>;

  constructor(read: () => Promise<Uint8Array>) {
    this.#read = read;
  }

  getReader() {
    if (this.locked) throw new TypeError("ReadableStream is locked");
    this.locked = true;
    let delivered = false;
    return {
      read: async () => delivered
        ? { value: undefined, done: true }
        : (delivered = true, { value: await this.#read(), done: false }),
      cancel: async () => { delivered = true; },
      releaseLock: () => { this.locked = false; },
      closed: Promise.resolve(),
    };
  }

  cancel() { return Promise.resolve(); }
}

function decodeUtf8(bytes: Uint8Array) {
  let output = "";
  for (let index = 0; index < bytes.length;) {
    const first = bytes[index++];
    if (first < 0x80) {
      output += String.fromCharCode(first);
      continue;
    }
    const length = first < 0xe0 ? 2 : first < 0xf0 ? 3 : 4;
    let codepoint = first & (0x7f >> length);
    let valid = length <= bytes.length - index + 1;
    for (let continuation = 1; continuation < length && valid; continuation++) {
      const byte = bytes[index++];
      valid = (byte & 0xc0) === 0x80;
      codepoint = (codepoint << 6) | (byte & 0x3f);
    }
    output += valid && codepoint <= 0x10ffff
      ? String.fromCodePoint(codepoint)
      : "\ufffd";
  }
  return output;
}

const host = globalThis as unknown as {
  performance: { now: () => number };
  queueMicrotask: (callback: () => void) => void;
  setTimeout: (callback: () => void, delay?: number) => number;
  clearTimeout: (id: number) => void;
  __liteSubscribe: (channel: string, callback: (payload: unknown) => void) => () => void;
  __liteTimer: (id: number) => void;
  __liteEvent: (channel: string, payload: unknown) => void;
  __liteFile: (descriptor: NativeFileDescriptor) => File;
  navigator: Navigator;
};

if (!("DOMException" in globalThis)) {
  class LiteDOMException extends Error {
    constructor(message = "", name = "Error") {
      super(message);
      this.name = name;
    }
  }
  (globalThis as unknown as { DOMException: typeof DOMException }).DOMException =
    LiteDOMException as unknown as typeof DOMException;
}

host.performance = {
  now: () => Number(globalThis.__liteNative("time.now", "")),
};
host.queueMicrotask = (callback) => { Promise.resolve().then(callback); };
host.setTimeout = (callback, delay = 0) => {
  const id = nextTimer++;
  timers.set(id, callback);
  globalThis.__liteNative("timer.set", `${id}:${delay}`);
  return id;
};
host.clearTimeout = (id) => {
  timers.delete(id);
  globalThis.__liteNative("timer.clear", String(id));
};
host.__liteSubscribe = (channel, callback) => {
  let subscribers = channels.get(channel);
  if (!subscribers) channels.set(channel, subscribers = new Set());
  subscribers.add(callback);
  return () => subscribers.delete(callback);
};
host.__liteTimer = (id) => {
  const callback = timers.get(id);
  timers.delete(id);
  callback?.();
};
host.__liteEvent = (channel, payload) => {
  if (channel === "clipboard") {
    const result = payload as { requestId: number; text: string };
    const resolve = clipboardReads.get(result.requestId);
    if (resolve) {
      clipboardReads.delete(result.requestId);
      resolve(result.text);
    }
  }
  for (const callback of channels.get(channel) ?? []) callback(payload);
};
host.__liteFile = (descriptor) => new LiteFile(descriptor) as unknown as File;

const clipboard: Clipboard = {
  readText: () => Promise.resolve().then(() => new Promise<string>((resolve) => {
    const requestId = Number(globalThis.__liteNative("clipboard.read", ""));
    clipboardReads.set(requestId, resolve);
  })),
  writeText: (text: string) => Promise.resolve().then(() => {
    if (typeof text !== "string") throw new TypeError("Clipboard text must be a string");
    globalThis.__liteNative("clipboard.write", text);
  }),
};
const navigatorValue = (globalThis as unknown as { navigator?: Navigator }).navigator
  ?? {} as Navigator;
Object.defineProperty(navigatorValue, "clipboard", {
  configurable: false,
  enumerable: true,
  value: clipboard,
});
host.navigator = navigatorValue;

// The native file remains lazy: createObjectURL retains only its descriptor and
// the audio worker opens/reads it on demand. Without the matching revoke call a
// replaced playlist would leak openable filesystem identities for the process.
(globalThis as unknown as { Blob: typeof Blob }).Blob = LiteBlob as unknown as typeof Blob;
(globalThis as unknown as { File: typeof File }).File = LiteFile as unknown as typeof File;
(globalThis as unknown as { ReadableStream: typeof ReadableStream }).ReadableStream ??=
  LiteReadableStream as unknown as typeof ReadableStream;
const urlStatics = {
  createObjectURL(value: Blob) {
    if (!(value instanceof LiteBlob)) throw new TypeError("createObjectURL requires a Blob");
    const url = globalThis.__liteNative(
      "fs.object-url-create",
      JSON.stringify(value.descriptor()),
    );
    liveObjectUrls.add(url);
    return url;
  },

  revokeObjectURL(url: string) {
    if (!liveObjectUrls.delete(url)) return;
    globalThis.__liteNative("fs.object-url-revoke", url);
  },
};
const existingUrl = (globalThis as unknown as { URL?: typeof URL }).URL;
if (existingUrl) {
  existingUrl.createObjectURL = urlStatics.createObjectURL;
  existingUrl.revokeObjectURL = urlStatics.revokeObjectURL;
} else {
  (globalThis as unknown as { URL: typeof URL }).URL =
    class LiteURL {
      static createObjectURL = urlStatics.createObjectURL;
      static revokeObjectURL = urlStatics.revokeObjectURL;
    } as unknown as typeof URL;
}
