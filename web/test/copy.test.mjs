import assert from 'node:assert/strict';
import { after, afterEach, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported. Node
// has a `navigator` of its own, which the window's replaces so the button reads the clipboard each case defines.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'HTMLDialogElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
after(() => window.close());
const { act, createElement: h, createRoot, CopyButton } = await import('./dist/ui.js');

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
  // jsdom defines neither, so a case's own stays behind unless it is removed.
  delete document.execCommand;
  delete navigator.clipboard;
});

/** The button a page renders under `host` for `text`. */
function render(text, host = document.body) {
  const container = document.createElement('div');
  host.append(container);
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(h(CopyButton, { text })));
  return container.querySelector('button');
}

/** A copy command that answers `answer`, counts its calls and records what `read` sees at the copy instant. */
function command(answer, read = () => null) {
  const calls = { count: 0, recorded: null };
  document.execCommand = (name) => {
    calls.count += 1;
    if (name === 'copy') calls.recorded = read();
    return answer;
  };
  return calls;
}

// jsdom reads "" through getSelection for a selected textarea, so the selection is read off the control itself.
const selected = () => {
  const el = document.activeElement;
  return el.value.slice(el.selectionStart, el.selectionEnd);
};
const clipboard = (writeText) => Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } });
// A clipboard write settles on a promise after a synchronous act has returned, so the wait steps out of the act
// environment for a few scheduler turns. An awaited act would keep the test process alive on its message channel.
async function settled() {
  globalThis.IS_REACT_ACT_ENVIRONMENT = false;
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setImmediate(resolve));
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
}
const click = async (button) => {
  act(() => button.click());
  await settled();
};

test('the copy command answers true and the button reads Copied with the text it was given', async () => {
  const calls = command(true, selected);
  const button = render('secret-123');
  await click(button);
  assert.equal(button.textContent.trim(), 'Copied');
  assert.equal(calls.recorded, 'secret-123', 'the command copied the text the button was given');
  assert.equal(calls.count, 1);
});

test('a copy command that answers false leaves the failed state', async () => {
  const calls = command(false);
  const button = render('secret-123');
  await click(button);
  assert.equal(button.textContent.trim(), 'Copy failed, select the text');
  assert.equal(button.getAttribute('title'), 'select the text and copy it by hand');
  assert.equal(calls.count, 1, 'the failed state came from the command, which ran once');
});

test('a clipboard whose write resolves never runs the command', async () => {
  clipboard(() => Promise.resolve());
  const calls = command(true);
  const button = render('secret-123');
  await click(button);
  assert.equal(button.textContent.trim(), 'Copied');
  assert.equal(calls.count, 0);
});

test('a clipboard whose write rejects copies through the command', async () => {
  clipboard(() => Promise.reject(new Error('denied')));
  const calls = command(true, selected);
  const button = render('secret-123');
  await click(button);
  assert.equal(button.textContent.trim(), 'Copied');
  assert.equal(calls.count, 1, 'the command ran once, after the write failed');
  assert.equal(calls.recorded, 'secret-123');
});

test("the command's textarea is appended inside the button's open dialog, and the focus returns to the button", async () => {
  // jsdom has no showModal, so the open attribute is set by hand.
  const dialog = document.createElement('dialog');
  dialog.setAttribute('open', '');
  document.body.append(dialog);
  const calls = command(true, () => document.activeElement.parentElement);
  const button = render('secret-123', dialog);
  await click(button);
  assert.equal(button.textContent.trim(), 'Copied');
  assert.equal(calls.recorded, dialog, 'the textarea sat inside the open dialog at the copy instant');
  assert.equal(document.activeElement, button, 'the focus is back on the button');
});
