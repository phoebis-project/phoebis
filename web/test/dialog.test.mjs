import assert from 'node:assert/strict';
import { after, afterEach, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'HTMLDialogElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
// jsdom has no showModal, so the shims keep the open attribute in step with the calls, as the smoke run's do.
window.HTMLDialogElement.prototype.showModal = function () {
  this.setAttribute('open', '');
};
window.HTMLDialogElement.prototype.close = function () {
  this.removeAttribute('open');
  this.dispatchEvent(new window.Event('close'));
};
after(() => window.close());
const { act, createElement: h, createRoot, Dialog } = await import('./dist/ui.js');

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
});

/**
 * An open dialog whose onClose counts its calls, with `unsaved` as a form page passes it. Its footer holds a Save
 * button, a second control after the X, so a case can tell the control that held the focus from the first one.
 */
function dialog(unsaved) {
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  roots.push(root);
  const closes = { count: 0 };
  act(() =>
    root.render(
      h(
        Dialog,
        {
          footer: h('button', { type: 'button' }, 'Save'),
          onClose: () => {
            closes.count += 1;
          },
          open: true,
          title: 'Edit',
          unsaved,
        },
        'body',
      ),
    ),
  );
  const element = container.querySelector('dialog');
  return { close: element.querySelector('.dialog__close'), closes, element, save: buttonIn(element, 'Save') };
}

/** The discard question, an open dialog of its own, or null while none is asked. */
const question = () => [...document.querySelectorAll('dialog[open]')].find((d) => d.textContent.includes('Discard changes?')) ?? null;
const buttonIn = (el, label) => [...el.querySelectorAll('button')].find((b) => b.textContent.trim() === label);
// A node compared through assert.equal is serialized whole on a mismatch, which never finishes on a jsdom tree, so
// the focus is asserted by identity with a short name of the element in the message.
const describe = (el) => (el ? `${el.tagName.toLowerCase()}${el.className ? `.${String(el.className).split(' ').join('.')}` : ''}` : 'none');
const focusedIs = (el, what) => assert.ok(document.activeElement === el, `${what}, not ${describe(document.activeElement)}`);
/** A click whose target is the dialog element itself, which is where a click on the backdrop lands. */
const clickOn = (el) => act(() => el.dispatchEvent(new window.MouseEvent('click', { bubbles: true })));
/** The cancel event the Escape key raises; answers whether a listener prevented it. */
const cancel = (el, cancelable) => {
  let prevented = false;
  act(() => {
    prevented = !el.dispatchEvent(new window.Event('cancel', { cancelable }));
  });
  return prevented;
};

test('an unchanged dialog closes on a click on its own element', () => {
  const { closes, element } = dialog(false);
  clickOn(element);
  assert.equal(closes.count, 1);
  assert.equal(question(), null, 'no question opens');
});

test('an unsaved dialog asks on the outside click and keeps editing', () => {
  const { close, closes, element, save } = dialog(true);
  act(() => save.focus());
  clickOn(element);
  const q = question();
  assert.ok(q, 'a second open dialog carries the question');
  assert.match(q.textContent, /This dialog has changes that are not saved\./);
  assert.equal(document.activeElement.textContent.trim(), 'Keep editing', 'the question opens with Keep editing focused');
  assert.equal(closes.count, 0);
  act(() => buttonIn(q, 'Keep editing').click());
  assert.equal(question(), null, 'Keep editing closes the question alone');
  assert.equal(closes.count, 0);
  assert.equal(element.hasAttribute('open'), true);
  focusedIs(save, 'the focus is back on the control that held it');
  assert.ok(document.activeElement !== close, 'the first control did not take it');
});

test('Keep editing hands the focus back to the last control the dialog held when the click took it away', () => {
  // A click on the backdrop moves the focus off the control before the dialog hears the click, as a mouse does.
  const { closes, element, save } = dialog(true);
  act(() => save.focus());
  act(() => save.blur());
  focusedIs(document.body, 'the focus left the dialog before the click');
  clickOn(element);
  assert.ok(question(), 'the dialog asks');
  act(() => buttonIn(question(), 'Keep editing').click());
  assert.equal(closes.count, 0);
  focusedIs(save, 'the last control the dialog held has the focus again');
});

test('Keep editing focuses the first control of the dialog when none held the focus', () => {
  const { close, element } = dialog(true);
  focusedIs(document.body, 'nothing holds the focus before the click');
  clickOn(element);
  act(() => buttonIn(question(), 'Keep editing').click());
  focusedIs(close, 'the first control a Tab reaches has the focus');
});

test('Discard closes the dialog', () => {
  const { closes, element } = dialog(true);
  clickOn(element);
  act(() => buttonIn(question(), 'Discard').click());
  assert.equal(closes.count, 1);
  assert.equal(question(), null);
});

test('the cancel event asks on an unsaved dialog and closes an unchanged one', () => {
  const unsaved = dialog(true);
  assert.equal(cancel(unsaved.element, true), true, 'the cancel is prevented, so the element stays open under the question');
  assert.ok(question(), 'the unsaved dialog asks');
  assert.equal(unsaved.closes.count, 0);
  act(() => buttonIn(question(), 'Keep editing').click());
  const unchanged = dialog(false);
  cancel(unchanged.element, true);
  assert.equal(unchanged.closes.count, 1, 'the unchanged dialog closes');
  assert.equal(question(), null);
});

test('an uncancelable cancel closes an unsaved dialog without asking', () => {
  const { closes, element } = dialog(true);
  cancel(element, false);
  assert.equal(closes.count, 1);
  assert.equal(question(), null);
});

test('an Escape on the question keeps editing', () => {
  const { closes, element } = dialog(true);
  clickOn(element);
  cancel(question(), true);
  assert.equal(question(), null, 'the question closed alone');
  assert.equal(closes.count, 0);
  assert.equal(element.hasAttribute('open'), true);
});
