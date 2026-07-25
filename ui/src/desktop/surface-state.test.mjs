import assert from "node:assert/strict";
import test from "node:test";
import { applySurfaceMove, reconcileSurfaces } from "./surface-state.js";

test("native reconciliation preserves the React-owned window frame", () => {
  const current = [
    { id: 1, title: "Old", bounds: { x: 40, y: 50, width: 500, height: 360 } },
  ];
  const native = [
    { id: 1, title: "New", bounds: { x: 150, y: 90, width: 720, height: 480 } },
    { id: 2, title: "Added", bounds: { x: 178, y: 114, width: 720, height: 480 } },
  ];

  assert.deepEqual(reconcileSurfaces(current, native), [
    { id: 1, title: "New", bounds: { x: 40, y: 50, width: 500, height: 360 } },
    native[1],
  ]);
});

test("completed native move changes position without restoring the old size", () => {
  const surfaces = [
    { id: 1, bounds: { x: 40, y: 50, width: 500, height: 360 } },
  ];

  assert.deepEqual(applySurfaceMove(surfaces, 1, 80, 100), [
    { id: 1, bounds: { x: 80, y: 100, width: 500, height: 360 } },
  ]);
});
