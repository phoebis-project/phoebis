import assert from 'node:assert/strict';
import { after, afterEach, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
after(() => window.close());
const { act, createElement: h, createRoot, fakeClock, installClock, useCommittedText } = await import('./dist/ui.js');

const ORIGIN = Date.UTC(2026, 8, 16, 14, 30);

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
  installClock(null);
});

/**
 * A frozen fake clock that also tracks the timeouts still armed, so a check can see a dropped timer directly and not
 * only through a commit it would have repeated.
 */
const frozen = () => {
  const clock = fakeClock();
  clock.freeze(ORIGIN);
  const armed = new Set();
  installClock({
    ...clock,
    clearTimeout: (id) => {
      armed.delete(id);
      clock.clearTimeout(id);
    },
    setTimeout: (fn, ms) => {
      const id = clock.setTimeout(() => {
        armed.delete(id);
        fn();
      }, ms);
      armed.add(id);
      return id;
    },
  });
  return { armed, tick: (ms) => act(() => clock.tick(ms)) };
};

/** A field over the hook, wired as the filter bar's search is: the draft in the field, the hook's handlers on it. */
function Text({ onCommit, value }) {
  const text = useCommittedText(value, onCommit);
  return h('input', { 'aria-label': 'Search', onBlur: text.onBlur, onChange: (e) => text.onChange(e.target.value), onKeyDown: text.onKeyDown, value: text.draft });
}

/**
 * One field under a page that hands it `value` and records every commit. `render` is the page rendering again with a
 * value, and with another handler where it passes one.
 */
function field(value = '') {
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  const commits = [];
  const render = (next, onCommit = (v) => commits.push(v)) => act(() => root.render(h(Text, { onCommit, value: next })));
  render(value);
  return { commits, input: container.querySelector('input'), render };
}

// React tracks an input's value through the prototype's setter, so a test sets it there and fires the input event a
// keystroke fires.
const setValue = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
const type = (input, text) =>
  act(() => {
    setValue.call(input, text);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
  });
const press = (input, key) => act(() => input.dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, key })));
const leave = (input) => {
  act(() => input.focus());
  act(() => input.blur());
};

test('leaving the field commits the draft at once and drops the pending commit', () => {
  const clock = frozen();
  const { commits, input } = field();
  type(input, 'curl/*');
  assert.equal(clock.armed.size, 1, 'the keystroke armed its commit');
  leave(input);
  assert.deepEqual(commits, ['curl/*'], 'the blur commits without waiting');
  assert.equal(clock.armed.size, 0, 'no commit stays armed behind the one the blur made');
  clock.tick(400);
  assert.deepEqual(commits, ['curl/*'], 'the committed value stands after the delay');
});

test('Enter commits the draft at once and drops the pending commit', () => {
  const clock = frozen();
  const { commits, input } = field();
  type(input, 'sdk/*');
  assert.equal(clock.armed.size, 1, 'the keystroke armed its commit');
  press(input, 'Enter');
  assert.deepEqual(commits, ['sdk/*'], 'Enter commits without waiting');
  assert.equal(clock.armed.size, 0, 'no commit stays armed behind the one Enter made');
  clock.tick(400);
  assert.deepEqual(commits, ['sdk/*'], 'the committed value stands after the delay');
  assert.equal(input.value, 'sdk/*');
});

test('typing commits once, the delay after the last keystroke, with the whole text', () => {
  const clock = frozen();
  const { commits, input } = field();
  type(input, 's');
  clock.tick(300);
  type(input, 'sd');
  clock.tick(300);
  type(input, 'sdk');
  clock.tick(399);
  assert.deepEqual(commits, [], 'nothing commits while the keystrokes keep coming');
  clock.tick(1);
  assert.deepEqual(commits, ['sdk'], 'one commit with the whole text, 400 ms after the last keystroke');
  clock.tick(400);
  assert.deepEqual(commits, ['sdk'], 'a later period commits nothing more');
  assert.equal(input.value, 'sdk');
});

test('a draft equal to the committed value commits nothing on blur, on Enter or after the delay', () => {
  const clock = frozen();
  const { commits, input } = field('sdk/*');
  leave(input);
  press(input, 'Enter');
  assert.deepEqual(commits, [], 'an untouched field commits nothing');
  type(input, 'sdk/');
  type(input, 'sdk/*');
  clock.tick(400);
  assert.deepEqual(commits, [], 'a draft typed back to the committed value commits nothing after the delay');
  leave(input);
  press(input, 'Enter');
  assert.deepEqual(commits, [], 'nor on a later blur or Enter');
  assert.equal(input.value, 'sdk/*');
});

test('a page echoing a commit back keeps the draft, even one typed past it', () => {
  const clock = frozen();
  const { commits, input, render } = field();
  type(input, 'ab');
  clock.tick(400);
  type(input, 'abc');
  // The page answers the commit late, after the next keystroke: its echo must not undo the keystroke.
  render('ab');
  assert.equal(input.value, 'abc', 'an echo of the last commit leaves the typing alone');
  clock.tick(400);
  assert.deepEqual(commits, ['ab', 'abc'], 'the text typed past the echo still commits');
  render('abc');
  assert.equal(input.value, 'abc', 'an echo equal to the text in the field changes nothing');
  clock.tick(400);
  assert.deepEqual(commits, ['ab', 'abc']);
});

test('a pending commit reaches the handler the page passed last, not the one it held at the keystroke', () => {
  const clock = frozen();
  const { commits, input, render } = field();
  type(input, 'sdk/*');
  // A page re-renders between the keystroke and the commit with a handler over its newer state.
  const later = [];
  render('', (v) => later.push(v));
  clock.tick(400);
  assert.deepEqual(commits, [], 'the handler the keystroke saw is never called');
  assert.deepEqual(later, ['sdk/*'], 'the commit goes to the handler of the last render');
});
