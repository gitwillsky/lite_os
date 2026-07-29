import assert from "node:assert/strict";
import test from "node:test";
import { cycle, openSwitcher, reconcileSwitcher, selectedCandidate } from "./app-switcher.ts";

const windows = (...ids) =>
  ids.map((id) => ({ id, title: `W${id}`, icon: `icon-${id}.png` }));

test("opening selects the previous window in MRU order", () => {
  // Z-order: 1 bottom, 3 current. Candidates reverse to MRU: [3, 2, 1].
  const state = openSwitcher(windows(1, 2, 3));
  assert.deepEqual(state.candidates.map((w) => w.id), [3, 2, 1]);
  // A bare Alt+Tab tap commits the previous window, not the current one.
  assert.equal(state.selection, 1);
  assert.equal(selectedCandidate(state).id, 2);
});

test("no windows means no panel", () => {
  assert.equal(openSwitcher([]), null);
});

test("a single window selects itself and Tab stays put", () => {
  const state = openSwitcher(windows(7));
  assert.equal(state.selection, 0);
  assert.equal(selectedCandidate(cycle(state, false)).id, 7);
  assert.equal(selectedCandidate(cycle(state, true)).id, 7);
});

test("Tab cycles forward and wraps past the oldest window", () => {
  let state = openSwitcher(windows(1, 2, 3));
  state = cycle(state, false);
  assert.equal(selectedCandidate(state).id, 1);
  state = cycle(state, false);
  assert.equal(selectedCandidate(state).id, 3);
});

test("Shift+Tab cycles backward and wraps past the current window", () => {
  let state = openSwitcher(windows(1, 2, 3));
  state = cycle(state, true);
  assert.equal(selectedCandidate(state).id, 3);
  state = cycle(state, true);
  assert.equal(selectedCandidate(state).id, 1);
});

test("a window closing mid-grab clamps the selection", () => {
  let state = openSwitcher(windows(1, 2, 3));
  state = cycle(state, false); // selection 2 (oldest window 1)
  // Window 1 closed: candidates shrink to [3, 2], selection clamps to 1.
  const clamped = reconcileSwitcher(state, windows(2, 3));
  assert.deepEqual(clamped.candidates.map((w) => w.id), [3, 2]);
  assert.equal(clamped.selection, 1);
});

test("losing every window closes the switcher without a commit", () => {
  const state = openSwitcher(windows(1));
  assert.equal(reconcileSwitcher(state, []), null);
});
