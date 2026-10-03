import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as d from './dist/api.js';

// A list of four items: 141 px tall and 160 px wide, so it needs 145 px with its 4 px gap.
const list = { height: 141, width: 160 };
const trigger = (top) => ({ bottom: top + 22, right: 900, top });
// A wrap 300 px tall inside a 1 px border, so its client box runs from 101 to 401; a horizontal scrollbar under it
// and the bottom border take the 16 px between that and the border box's 417.
const wrap = { clientHeight: 300, clientTop: 1, getBoundingClientRect: () => ({ bottom: 417, top: 100 }) };

test('a menu without a clipping wrap opens downward while the viewport holds the list below its trigger', () => {
  assert.deepEqual(d.menuPlacement(trigger(300), null, list, 800), { kind: 'down' });
});

test("a menu reads the room below against the wrap's client box, so its scrollbar never hides the last item", () => {
  // The border box leaves 145 px below the trigger, the client box 129: the list goes up, where 149 px are free.
  assert.deepEqual(d.menuPlacement(trigger(250), wrap, list, 800), { kind: 'up' });
  assert.deepEqual(d.menuPlacement(trigger(200), wrap, list, 800), { kind: 'down' });
});

test("a menu reads the room above from the wrap's client box or the viewport's top, whichever is lower", () => {
  // The wrap's client box starts 199 px above the viewport and ends at 200.
  const high = { clientHeight: 399, clientTop: 1, getBoundingClientRect: () => ({ bottom: 201, top: -200 }) };
  assert.deepEqual(d.menuPlacement(trigger(100), high, list, 800), { kind: 'float', left: 740, top: 126 });
  assert.deepEqual(d.menuPlacement(trigger(150), high, list, 800), { kind: 'up' });
});

test('a menu with room on neither side of its row inside the wrap floats at its trigger, right-aligned', () => {
  const short = { clientHeight: 80, clientTop: 1, getBoundingClientRect: () => ({ bottom: 182, top: 100 }) };
  assert.deepEqual(d.menuPlacement(trigger(140), short, list, 800), { kind: 'float', left: 740, top: 166 });
});

test('a floating menu goes above its trigger when the viewport has no room below it', () => {
  const short = { clientHeight: 80, clientTop: 1, getBoundingClientRect: () => ({ bottom: 782, top: 700 }) };
  assert.deepEqual(d.menuPlacement(trigger(740), short, list, 800), { kind: 'float', left: 740, top: 595 });
});

test("a menu at a row's start floats aligned to its trigger's left edge, as its list in flow opens", () => {
  const short = { clientHeight: 80, clientTop: 1, getBoundingClientRect: () => ({ bottom: 182, top: 100 }) };
  assert.deepEqual(d.menuPlacement({ ...trigger(140), left: 40 }, short, list, 800, 'left'), { kind: 'float', left: 40, top: 166 });
  assert.deepEqual(d.floatAt({ ...trigger(740), left: 40 }, list, 800, 'left'), { kind: 'float', left: 40, top: 595 });
});

test('a wrap that runs past the viewport bounds the list by the viewport', () => {
  const tall = { clientHeight: 900, clientTop: 1, getBoundingClientRect: () => ({ bottom: 1002, top: 100 }) };
  assert.deepEqual(d.menuPlacement(trigger(700), tall, list, 800), { kind: 'up' });
});
