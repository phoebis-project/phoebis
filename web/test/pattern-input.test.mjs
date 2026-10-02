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
const { act, createElement: h, createRoot, fakeClock, installClock, PATTERN_GRAMMAR, PatternInput } = await import('./dist/ui.js');

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

/** One field under a page that hands it `value` and records every commit; `render` is the page passing a new value. */
function field(value = '') {
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  const commits = [];
  const render = (next) => act(() => root.render(h(PatternInput, { label: 'Client pattern', onCommit: (v) => commits.push(v), value: next })));
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

test('the field names itself by its label and carries the pattern grammar in its title', () => {
  frozen();
  const { input } = field();
  assert.equal(input.getAttribute('placeholder'), 'Client pattern');
  assert.equal(input.getAttribute('aria-label'), 'Client pattern');
  assert.equal(input.getAttribute('title'), PATTERN_GRAMMAR);
  assert.equal(PATTERN_GRAMMAR, '* matches any run of characters, ? one; a backslash quotes either');
  assert.ok(input.classList.contains('mono'), 'the pattern reads in the monospace face');
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

test('Enter commits at once and drops the pending commit', () => {
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

test('leaving the field commits at once', () => {
  const clock = frozen();
  const { commits, input } = field();
  type(input, 'curl/*');
  act(() => input.focus());
  act(() => input.blur());
  assert.deepEqual(commits, ['curl/*'], 'the blur commits without waiting');
  assert.equal(clock.armed.size, 0);
  clock.tick(400);
  assert.deepEqual(commits, ['curl/*']);
});

test('a value from outside replaces the draft without a commit', () => {
  const clock = frozen();
  const { commits, input, render } = field('sdk/*');
  assert.equal(input.value, 'sdk/*', 'the field opens on the committed value');
  render('');
  assert.equal(input.value, '', 'a reset clears the field');
  clock.tick(400);
  assert.deepEqual(commits, [], 'a value the page set is not committed back to it');
});

test('a value from outside drops a pending edit, which would otherwise restore the text it replaced', () => {
  const clock = frozen();
  const { commits, input, render } = field();
  type(input, 'cl');
  type(input, 'cla');
  render('claude-cli/*');
  assert.equal(input.value, 'claude-cli/*', 'the navigation lands its value in the field');
  clock.tick(400);
  assert.deepEqual(commits, [], 'the edit typed before the navigation never commits');
  assert.equal(input.value, 'claude-cli/*');
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
