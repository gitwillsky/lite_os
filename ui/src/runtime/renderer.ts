import React from "react";
import Reconciler from "react-reconciler";
import "./platform.ts";
import { LiteMediaElement } from "./media.ts";

/** A rasterizable scene node the host serializes to the compositor. */
interface Instance {
  id: number;
  type: string;
  props: Record<string, unknown>;
  text?: string;
  hidden?: boolean;
  children: Instance[];
  media?: LiteMediaElement;
}
type Props = Record<string, unknown>;

const primitives = new Set(["div", "span", "img", "input", "button", "audio"]);
const listeners = new Map<number, (payload: unknown) => void>();
let nextListener = 1;
let nextNode = 1;
const container: { children: Instance[] } = { children: [] };
const hostContext = {};
// The un-encoded source props per instance (functions intact), needed on update
// to diff listener identities. Kept off the serialized instance in a side table.
const sourceProps = new WeakMap<Instance, Props>();

function encodeProps(props: Props, previousEncoded: Props = {}): Props {
  const encoded: Props = {};
  for (const [name, value] of Object.entries(props)) {
    if (name === "children") continue;
    if (typeof value === "function") {
      // Listener identity belongs to the stable host node + prop slot, not to
      // the JavaScript function object created by a particular React render.
      // Reusing the id keeps the latest native hit snapshot dispatchable while
      // React replaces inline callbacks; allocating a new id here creates a
      // window where native still holds the old id after it has been deleted.
      const previousListener = previousEncoded[name];
      const listener = typeof previousListener === "number" ? previousListener : nextListener++;
      listeners.set(listener, value as (payload: unknown) => void);
      encoded[name] = listener;
    } else {
      encoded[name] = value;
    }
  }
  return encoded;
}

function publish() {
  const scene = JSON.stringify(container.children, (name, value) => {
    if (name === "media") return undefined;
    return value;
  });
  globalThis.__liteNative("scene.commit", scene);
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

function addListener(callback: (payload: unknown) => void) {
  const id = nextListener++;
  listeners.set(id, callback);
  return id;
}

const reconciler = Reconciler({
  supportsMutation: true,
  supportsPersistence: false,
  supportsHydration: false,
  isPrimaryRenderer: true,
  warnsIfNotActing: false,
  getRootHostContext: () => hostContext,
  getChildHostContext: () => hostContext,
  getPublicInstance: (instance: Instance) => instance.media ?? instance,
  prepareForCommit: () => null,
  resetAfterCommit: publish,
  createInstance(type: string, props: Props) {
    if (!primitives.has(type)) throw new Error(`unsupported LiteUI primitive '${type}'`);
    const instance: Instance = { id: nextNode++, type: type === "audio" ? "div" : type, props: encodeProps(props), children: [] };
    if (type === "audio") {
      instance.media = new LiteMediaElement(() => {
        instance.children = instance.media?.shadowChildren() as Instance[] ?? [];
        publish();
      }, addListener, () => nextNode++);
      instance.media.updateProps(props);
      instance.children = instance.media.shadowChildren() as Instance[];
      instance.props = {
        ...instance.props,
        style: {
          display: "flex", alignItems: "center", gap: 3, width: 420, height: 32,
          padding: 3, border: "2px inset #ffffff", background: "#d4d0c8",
          ...(props.style as Record<string, unknown> ?? {}),
        },
      };
    }
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
      if (typeof value === "function" && typeof newProps[name] !== "function") {
        listeners.delete(instance.props[name] as number);
      }
    }
    instance.props = encodeProps(newProps, instance.props);
    if (instance.media) {
      instance.media.updateProps(newProps);
      instance.children = instance.media.shadowChildren() as Instance[];
      instance.props.style = {
        display: "flex", alignItems: "center", gap: 3, width: 420, height: 32,
        padding: 3, border: "2px inset #ffffff", background: "#d4d0c8",
        ...(newProps.style as Record<string, unknown> ?? {}),
      };
    }
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
  detachDeletedInstance(instance: Instance) {
    instance.media?.destroy();
    dropListeners(instance);
  },
  requestPostPaintCallback: (callback: (time: number) => void) => callback(performance.now()),
  resetFormInstance: () => {},
});

globalThis.__liteDispatch = (listener: number | readonly number[], payload: unknown) => {
  const route = typeof listener === "number" ? [listener] : listener;
  let propagationStopped = false;
  const event = typeof payload === "object" && payload !== null
    ? payload as Record<string, unknown>
    : { value: payload };
  Object.defineProperty(event, "stopPropagation", {
    configurable: true,
    value: () => { propagationStopped = true; },
  });
  Object.defineProperty(event, "stopImmediatePropagation", {
    configurable: true,
    value: () => { propagationStopped = true; },
  });

  reconciler.discreteUpdates(() => {
    for (const id of route) {
      listeners.get(id)?.(event);
      if (propagationStopped) {
        break;
      }
    }
  });
};

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
