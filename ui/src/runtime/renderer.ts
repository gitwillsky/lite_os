import React from "react";
import Reconciler from "react-reconciler";
import "./platform.ts";

/** A rasterizable scene node the host serializes to the compositor. */
interface Instance {
  id: number;
  type: string;
  props: Record<string, unknown>;
  text?: string;
  hidden?: boolean;
  children: Instance[];
}
type Props = Record<string, unknown>;

const primitives = new Set(["div", "span", "img", "input"]);
const listeners = new Map<number, (payload: unknown) => void>();
let nextListener = 1;
let nextNode = 1;
const container: { children: Instance[] } = { children: [] };
const hostContext = {};
// The un-encoded source props per instance (functions intact), needed on update
// to diff listener identities. Kept off the serialized instance in a side table.
const sourceProps = new WeakMap<Instance, Props>();

function encodeProps(props: Props, previousProps: Props = {}, previousEncoded: Props = {}): Props {
  const encoded: Props = {};
  for (const [name, value] of Object.entries(props)) {
    if (name === "children") continue;
    if (typeof value === "function") {
      const listener = previousProps[name] === value
        ? (previousEncoded[name] as number)
        : nextListener++;
      if (previousProps[name] !== value) listeners.set(listener, value as (payload: unknown) => void);
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

function remove(parent: { children: Instance[] }, child: Instance) {
  const index = parent.children.indexOf(child);
  if (index >= 0) parent.children.splice(index, 1);
}

function dropOwnListeners(instance: Instance) {
  for (const [name, value] of Object.entries(instance.props ?? {})) {
    if (name.startsWith("on") && typeof value === "number") listeners.delete(value);
  }
}

function dropListeners(instance: Instance) {
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
  getPublicInstance: (instance: Instance) => instance,
  prepareForCommit: () => null,
  resetAfterCommit: publish,
  createInstance(type: string, props: Props) {
    if (!primitives.has(type)) throw new Error(`unsupported LiteUI primitive '${type}'`);
    const instance: Instance = { id: nextNode++, type, props: encodeProps(props), children: [] };
    sourceProps.set(instance, props);
    return instance;
  },
  appendInitialChild: (parent: Instance, child: Instance) => parent.children.push(child),
  finalizeInitialChildren: () => false,
  shouldSetTextContent: () => false,
  createTextInstance: (text: string): Instance => ({ id: nextNode++, type: "#text", props: {}, text: String(text), children: [] }),
  scheduleTimeout: setTimeout,
  cancelTimeout: clearTimeout,
  noTimeout: -1,
  supportsMicrotasks: true,
  scheduleMicrotask: queueMicrotask,
  appendChild: (parent: Instance, child: Instance) => { remove(parent, child); parent.children.push(child); },
  appendChildToContainer: (parent: { children: Instance[] }, child: Instance) => { remove(parent, child); parent.children.push(child); },
  insertBefore(parent: Instance, child: Instance, before: Instance) {
    remove(parent, child);
    const index = parent.children.indexOf(before);
    parent.children.splice(index < 0 ? parent.children.length : index, 0, child);
  },
  insertInContainerBefore(parent: { children: Instance[] }, child: Instance, before: Instance) {
    remove(parent, child);
    const index = parent.children.indexOf(before);
    parent.children.splice(index < 0 ? parent.children.length : index, 0, child);
  },
  removeChild: remove,
  removeChildFromContainer: remove,
  clearContainer: (parent: { children: Instance[] }) => { parent.children.length = 0; },
  commitUpdate(instance: Instance, type: string, oldProps: Props, newProps: Props) {
    const previous = sourceProps.get(instance) ?? {};
    for (const [name, value] of Object.entries(previous)) {
      if (typeof value === "function" && newProps[name] !== value) {
        listeners.delete(instance.props[name] as number);
      }
    }
    instance.props = encodeProps(newProps, previous, instance.props);
    sourceProps.set(instance, newProps);
  },
  commitTextUpdate(instance: Instance, oldText: string, newText: string) { instance.text = String(newText); },
  resetTextContent: () => {},
  hideInstance: (instance: Instance) => { instance.props.hidden = true; },
  unhideInstance: (instance: Instance) => { delete instance.props.hidden; },
  hideTextInstance: (instance: Instance) => { instance.hidden = true; },
  unhideTextInstance: (instance: Instance) => { instance.hidden = false; },
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
  requestPostPaintCallback: (callback: (time: number) => void) => callback(performance.now()),
  resetFormInstance: () => {},
});

globalThis.__liteDispatch = (listener: number, payload: unknown) => listeners.get(listener)?.(payload);

/** Mounts the bundle's only React root into the native LiteUI scene seam. */
export function mount(Component: React.ComponentType) {
  const root = reconciler.createContainer(
    container,
    0,
    null,
    false,
    null,
    "lite-ui",
    (error: unknown) => { throw error; },
    (error: unknown) => { throw error; },
    (error: unknown) => { throw error; },
    null,
  );
  reconciler.updateContainer(React.createElement(Component), root, null, null);
}
