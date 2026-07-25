// Minimal platform polyfills backing the React runtime on the LiteOS host:
// timers and microtasks route through the native bridge, and a channel
// registry powers `__liteSubscribe`/`__liteEvent`. These assign host globals
// whose lib signatures don't match the compositor ABI, so the shim object is
// installed through an `any` view of globalThis by design.
const timers = new Map<number, () => void>();
const channels = new Map<string, Set<(payload: unknown) => void>>();
let nextTimer = 1;

const host = globalThis as unknown as {
  performance: { now: () => number };
  queueMicrotask: (callback: () => void) => void;
  setTimeout: (callback: () => void, delay?: number) => number;
  clearTimeout: (id: number) => void;
  __liteSubscribe: (channel: string, callback: (payload: unknown) => void) => () => void;
  __liteTimer: (id: number) => void;
  __liteEvent: (channel: string, payload: unknown) => void;
};

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
  for (const callback of channels.get(channel) ?? []) callback(payload);
};
