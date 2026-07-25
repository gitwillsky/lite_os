import assert from "node:assert/strict";
import test from "node:test";
import { constrainResize, projectResize } from "./window-geometry.js";

const origin = { startX: 200, startY: 200, x: 100, y: 80, width: 400, height: 300 };
const point = { x: 230, y: 250 };

test("all eight XP grips keep their opposite edges fixed", () => {
  const expected = {
    n: [100, 130, 400, 250],
    s: [100, 80, 400, 350],
    e: [100, 80, 430, 300],
    w: [130, 80, 370, 300],
    ne: [100, 130, 430, 250],
    nw: [130, 130, 370, 250],
    se: [100, 80, 430, 350],
    sw: [130, 80, 370, 350],
  };
  for (const [edge, frame] of Object.entries(expected)) {
    const actual = projectResize(edge, origin, point);
    assert.deepEqual([actual.x, actual.y, actual.width, actual.height], frame);
  }
});

test("north and west work-area clamps preserve the far edge", () => {
  const workArea = { x: 0, y: 0, width: 1504, height: 816 };
  assert.deepEqual(
    constrainResize(
      { x: -20, y: -30, width: 520, height: 430, anchorRight: true, anchorBottom: true, right: 500, bottom: 400 },
      workArea,
      160,
      120,
    ),
    { x: 0, y: 0, width: 500, height: 400 },
  );
});

test("minimum size pins the opposite edge", () => {
  const workArea = { x: 0, y: 0, width: 1504, height: 816 };
  assert.deepEqual(
    constrainResize(
      { x: 480, y: 360, width: 20, height: 40, anchorRight: true, anchorBottom: true, right: 500, bottom: 400 },
      workArea,
      160,
      120,
    ),
    { x: 340, y: 280, width: 160, height: 120 },
  );
});
