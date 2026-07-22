import React from "react";
import Reconciler from "react-reconciler";
import "./platform.js";

const primitives = new Set(["view", "text", "image", "text-input", "surface"]);
const listeners = new Map();
let nextListener = 1;
const container = { children: [] };
const hostContext = {};
const sourceProps = Symbol("sourceProps");

function encodeProps(props, previousProps = {}, previousEncoded = {}) {
  const encoded = {};
  for (const [name, value] of Object.entries(props)) {
    if (name === "children") continue;
    if (typeof value === "function") {
      const listener = previousProps[name] === value
        ? previousEncoded[name]
        : nextListener++;
      if (previousProps[name] !== value) listeners.set(listener, value);
      encoded[name] = listener;
    } else {
      encoded[name] = value;
    }
  }
  return encoded;
}

function publish() {
  globalThis.__liteNative("scene.commit", JSON.stringify(container.children));
}

function remove(parent, child) {
  const index = parent.children.indexOf(child);
  if (index >= 0) parent.children.splice(index, 1);
}

function dropOwnListeners(instance) {
  for (const [name, value] of Object.entries(instance.props ?? {})) {
    if (name.startsWith("on") && typeof value === "number") listeners.delete(value);
  }
}

function dropListeners(instance) {
  dropOwnListeners(instance);
  for (const child of instance.children ?? []) dropListeners(child);
}

const reconciler = Reconciler({
  supportsMutation: true,
  supportsPersistence: false,
  supportsHydration: false,
  isPrimaryRenderer: true,
  warnsIfNotActing: false,
  getRootHostContext: () => hostContext,
  getChildHostContext: () => hostContext,
  getPublicInstance: (instance) => instance,
  prepareForCommit: () => null,
  resetAfterCommit: publish,
  createInstance(type, props) {
    if (!primitives.has(type)) throw new Error(`unsupported LiteUI primitive '${type}'`);
    const instance = { type, props: encodeProps(props), children: [] };
    Object.defineProperty(instance, sourceProps, { value: props, writable: true });
    return instance;
  },
  appendInitialChild: (parent, child) => parent.children.push(child),
  finalizeInitialChildren: () => false,
  shouldSetTextContent: () => false,
  createTextInstance: (text) => ({ type: "#text", text: String(text), children: [] }),
  scheduleTimeout: setTimeout,
  cancelTimeout: clearTimeout,
  noTimeout: -1,
  supportsMicrotasks: true,
  scheduleMicrotask: queueMicrotask,
  appendChild: (parent, child) => parent.children.push(child),
  appendChildToContainer: (parent, child) => parent.children.push(child),
  insertBefore(parent, child, before) {
    remove(parent, child);
    parent.children.splice(parent.children.indexOf(before), 0, child);
  },
  insertInContainerBefore(parent, child, before) {
    remove(parent, child);
    parent.children.splice(parent.children.indexOf(before), 0, child);
  },
  removeChild: remove,
  removeChildFromContainer: remove,
  clearContainer: (parent) => { parent.children.length = 0; },
  commitUpdate(instance, type, oldProps, newProps) {
    for (const [name, value] of Object.entries(instance[sourceProps])) {
      if (typeof value === "function" && newProps[name] !== value) {
        listeners.delete(instance.props[name]);
      }
    }
    instance.props = encodeProps(newProps, instance[sourceProps], instance.props);
    instance[sourceProps] = newProps;
  },
  commitTextUpdate(instance, oldText, newText) { instance.text = String(newText); },
  resetTextContent: () => {},
  hideInstance: (instance) => { instance.props.hidden = true; },
  unhideInstance: (instance) => { delete instance.props.hidden; },
  hideTextInstance: (instance) => { instance.hidden = true; },
  unhideTextInstance: (instance) => { instance.hidden = false; },
  maySuspendCommit: () => false,
  preloadInstance: () => true,
  startSuspendingCommit: () => {},
  suspendInstance: () => {},
  waitForCommitToBeReady: () => null,
  NotPendingTransition: null,
  HostTransitionContext: React.createContext(null),
  setCurrentUpdatePriority: () => {},
  getCurrentUpdatePriority: () => 2,
  resolveUpdatePriority: () => 2,
  trackSchedulerEvent: () => {},
  resolveEventType: () => null,
  resolveEventTimeStamp: () => -1.1,
  resolveEventPriority: () => 2,
  shouldAttemptEagerTransition: () => false,
  detachDeletedInstance: dropListeners,
  requestPostPaintCallback: (callback) => callback(performance.now()),
  resetFormInstance: () => {},
});

globalThis.__liteDispatch = (listener, payload) => listeners.get(listener)?.(payload);

/** Mounts the bundle's only React root into the native LiteUI scene seam. */
export function mount(Component) {
  const root = reconciler.createContainer(
    container,
    0,
    null,
    false,
    null,
    "lite-ui",
    (error) => { throw error; },
    (error) => { throw error; },
    (error) => { throw error; },
    null,
  );
  reconciler.updateContainer(React.createElement(Component), root, null, null);
}
