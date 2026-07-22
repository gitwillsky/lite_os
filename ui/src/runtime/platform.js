const timers = new Map();
const channels = new Map();
let nextTimer = 1;

globalThis.performance = {
  now: () => Number(globalThis.__liteNative("time.now", "")),
};
globalThis.queueMicrotask = (callback) => Promise.resolve().then(callback);
globalThis.setTimeout = (callback, delay = 0) => {
  const id = nextTimer++;
  timers.set(id, callback);
  globalThis.__liteNative("timer.set", `${id}:${delay}`);
  return id;
};
globalThis.clearTimeout = (id) => {
  timers.delete(id);
  globalThis.__liteNative("timer.clear", String(id));
};
globalThis.__liteSubscribe = (channel, callback) => {
  let subscribers = channels.get(channel);
  if (!subscribers) channels.set(channel, subscribers = new Set());
  subscribers.add(callback);
  return () => subscribers.delete(callback);
};
globalThis.__liteTimer = (id) => {
  const callback = timers.get(id);
  timers.delete(id);
  callback?.();
};
globalThis.__liteEvent = (channel, payload) => {
  for (const callback of channels.get(channel) ?? []) callback(payload);
};
