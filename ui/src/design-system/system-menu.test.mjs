import test from "node:test";
import assert from "node:assert/strict";
import { systemMenuItems } from "./system-menu.ts";

const item = (items, id) => items.find((entry) => entry.id === id);

test("a normal window disables restore and keeps move/size placeholders disabled", () => {
  const items = systemMenuItems({ minimized: false, maximized: false });
  assert.equal(item(items, "restore").disabled, true);
  assert.equal(item(items, "move").disabled, true);
  assert.equal(item(items, "size").disabled, true);
  assert.equal(item(items, "minimize").disabled, false);
  assert.equal(item(items, "maximize").disabled, false);
  assert.equal(item(items, "close").disabled, false);
});

test("a minimized window offers restore and maximize but not minimize", () => {
  const items = systemMenuItems({ minimized: true, maximized: false });
  assert.equal(item(items, "restore").disabled, false);
  assert.equal(item(items, "minimize").disabled, true);
  assert.equal(item(items, "maximize").disabled, false);
});

test("a maximized window offers restore and minimize but not maximize", () => {
  const items = systemMenuItems({ minimized: false, maximized: true });
  assert.equal(item(items, "restore").disabled, false);
  assert.equal(item(items, "minimize").disabled, false);
  assert.equal(item(items, "maximize").disabled, true);
});

test("the menu keeps the XP six-row shape with one separator before close", () => {
  const items = systemMenuItems({ minimized: false, maximized: false });
  assert.deepEqual(
    items.map((entry) => entry.id),
    ["restore", "move", "size", "minimize", "maximize", "separator", "close"],
  );
  assert.equal(items[5].separator, true);
});
