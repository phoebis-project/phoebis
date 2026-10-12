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
const { act, AddFilter, Button, ChipLine, createElement: h, createRoot, FilterChip, PATTERN_GRAMMAR, Popover, PopoverHost, useState } = await import('./dist/ui.js');
const { chipsOf, emptyValue, KEYS_DIMENSIONS } = await import('./dist/api.js');

// jsdom lays nothing out, so the boxes the placement reads are stood in: a 1280 by 800 window, an anchor's box from
// its `data-rect` (left, top, width, height), and the popover's own size wherever its style puts it.
let boxSize = { height: 100, width: 200 };
const rect = (left, top, width, height) => ({ bottom: top + height, height, left, right: left + width, top, width, x: left, y: top });
window.HTMLElement.prototype.getBoundingClientRect = function () {
  if (this.classList.contains('popover')) return rect(parseFloat(this.style.left) || 0, parseFloat(this.style.top) || 0, boxSize.width, boxSize.height);
  if (this.dataset.rect) return rect(...this.dataset.rect.split(',').map(Number));
  return rect(0, 0, 0, 0);
};
Object.defineProperty(document.documentElement, 'clientWidth', { configurable: true, value: 1280 });
Object.defineProperty(document.documentElement, 'clientHeight', { configurable: true, value: 800 });

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
  boxSize = { height: 100, width: 200 };
});

const render = (node) => {
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(node));
  return { container, render: (next) => act(() => root.render(next)) };
};

const press = (key, target = document.activeElement, init = {}) => {
  const event = new window.KeyboardEvent('keydown', { bubbles: true, cancelable: true, key, ...init });
  act(() => target.dispatchEvent(event));
  return event;
};
const mousedown = (target) => act(() => target.dispatchEvent(new window.MouseEvent('mousedown', { bubbles: true })));
const focus = (el) => act(() => el.focus());
const click = (el) => act(() => el.click());
// React tracks an input's value through the prototype's setter, so a test sets it there and fires the input event a
// keystroke fires.
const setValue = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
const type = (input, text) =>
  act(() => {
    setValue.call(input, text);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
  });
const box = () => document.querySelector('.popover');

/** A Button that opens a popover on itself, with a button elsewhere on the page to move focus to. */
function Opened({ children, labelledBy = 'head', onClose, placeKey, rectOf = '40,70,96,30' }) {
  const [anchor, setAnchor] = useState(null);
  return h(
    'div',
    null,
    h(Button, { 'data-rect': rectOf, ref: setAnchor }, 'Open'),
    h('button', { className: 'elsewhere', type: 'button' }, 'Elsewhere'),
    h(Popover, { anchor, id: 'pop', labelledBy, onClose, open: true, placeKey }, children),
  );
}
const content = () => [h('span', { id: 'head', key: 'head' }, 'Title'), h('input', { 'aria-label': 'Field', key: 'field' }), h('button', { key: 'go', type: 'button' }, 'Go')];

test('the box portals to the body as a dialog with aria-modal false and its labelled-by id, and focus moves to its first tabbable, a native field before a button', () => {
  render(h(Opened, { onClose() {} }, content()));
  const el = box();
  assert.ok(el?.parentElement === document.body, 'the box is a child of the body');
  assert.deepEqual([el.id, el.getAttribute('role'), el.getAttribute('aria-modal'), el.getAttribute('aria-labelledby'), el.getAttribute('tabindex')], ['pop', 'dialog', 'false', 'head', '-1']);
  assert.ok(document.activeElement === el.querySelector('input'), 'the field before the button takes focus');
});

test("a popover anchored on a Button's forwarded ref hangs 8 px under it, left edges aligned, and Escape returns focus to it", () => {
  const closes = [];
  const { container } = render(h(Opened, { onClose: () => closes.push('close') }, content()));
  const anchor = container.querySelector('.btn');
  const el = box();
  assert.ok(el, "the box draws once the Button's ref names its element");
  assert.deepEqual([el.style.position, el.style.left, el.style.top, el.style.visibility, el.style.maxHeight], ['fixed', '40px', '108px', 'visible', '784px']);
  press('Escape');
  assert.ok(document.activeElement === anchor, 'Escape puts focus on the Button');
  assert.deepEqual(closes, ['close']);
});

test("the box flips above an anchor near the window's foot and keeps 8 px inside the window", () => {
  render(h(Opened, { onClose() {}, rectOf: '1200,700,60,30' }, content()));
  assert.deepEqual([box().style.left, box().style.top], ['1072px', '592px'], 'kept 8 px from the right edge and drawn 8 px above the anchor');
  act(() => roots.pop().unmount());
  boxSize = { height: 784, width: 200 };
  render(h(Opened, { onClose() {}, rectOf: '40,700,60,30' }, content()));
  assert.equal(box().style.top, '8px', 'a box as tall as the window over a low anchor starts 8 px under its top');
});

test('a PopoverHost above the box sees add on open and remove on close and on unmount', () => {
  const calls = [];
  const host = { add: (id) => calls.push(`add ${id}`), remove: (id) => calls.push(`remove ${id}`) };
  function Toggled() {
    const [anchor, setAnchor] = useState(null);
    const [open, setOpen] = useState(true);
    return h(PopoverHost.Provider, { value: host }, h('button', { className: 'toggle', onClick: () => setOpen((v) => !v), ref: setAnchor, type: 'button' }, 'Open'), h(Popover, { anchor, id: 'menu', labelledBy: 'x', onClose: () => setOpen(false), open }, h('button', { type: 'button' }, 'In')));
  }
  const { container } = render(h(Toggled));
  assert.deepEqual(calls, ['add menu']);
  click(container.querySelector('.toggle'));
  assert.deepEqual(calls, ['add menu', 'remove menu'], 'closing removes the id');
  click(container.querySelector('.toggle'));
  act(() => roots.pop().unmount());
  assert.deepEqual(calls, ['add menu', 'remove menu', 'add menu', 'remove menu'], 'unmounting while open removes it too');
});

test('Escape calls onClose once, puts focus on the anchor and goes no further', () => {
  const closes = [];
  const heard = [];
  const listen = (e) => heard.push(e.key);
  document.addEventListener('keydown', listen);
  try {
    const { container } = render(h(Opened, { onClose: () => closes.push('close') }, content()));
    const escape = press('Escape');
    assert.deepEqual(closes, ['close'], 'onClose runs once');
    assert.ok(document.activeElement === container.querySelector('.btn'), 'focus is on the anchor');
    assert.ok(escape.defaultPrevented && heard.length === 0, 'the box takes the key, so a drawer under it stays open');
  } finally {
    document.removeEventListener('keydown', listen);
  }
});

test('a mousedown outside the box and its anchor closes it, and one on the anchor or inside the box does not', () => {
  const closes = [];
  const { container } = render(h(Opened, { onClose: () => closes.push('close') }, content()));
  mousedown(container.querySelector('.btn'));
  mousedown(box().querySelector('button'));
  assert.deepEqual(closes, [], 'the anchor and the box keep it open');
  mousedown(document.body);
  assert.deepEqual(closes, ['close'], 'the body outside closes it');
});

test('focus leaving for an element outside closes the box, and focus moving to the anchor does not', () => {
  const closes = [];
  const { container } = render(h(Opened, { onClose: () => closes.push('close') }, content()));
  focus(container.querySelector('.btn'));
  assert.deepEqual(closes, [], 'the anchor keeps it open');
  focus(box().querySelector('input'));
  focus(container.querySelector('.elsewhere'));
  assert.deepEqual(closes, ['close'], 'an element outside closes it');
});

test('Tab past the last stop moves on to the stop after the anchor and Shift+Tab before the first returns to the anchor, each closing the box once', () => {
  const closes = [];
  const { container } = render(h(Opened, { onClose: () => closes.push('close') }, content()));
  const [field, go] = [box().querySelector('input'), box().querySelector('button')];
  const inside = press('Tab', field);
  assert.ok(!inside.defaultPrevented && closes.length === 0, "a Tab between the stops is the browser's own");
  focus(go);
  const out = press('Tab', go);
  assert.ok(out.defaultPrevented && document.activeElement === container.querySelector('.elsewhere'), 'Tab off the last stop lands on the stop after the anchor, not past the page');
  assert.deepEqual(closes, ['close']);
  act(() => roots.pop().unmount());
  closes.length = 0;
  const again = render(h(Opened, { onClose: () => closes.push('close') }, content()));
  const back = press('Tab', box().querySelector('input'), { shiftKey: true });
  assert.ok(back.defaultPrevented && document.activeElement === again.container.querySelector('.btn'), 'Shift+Tab off the first stop lands on the anchor');
  assert.deepEqual(closes, ['close']);
});

test('a box whose anchor has left the document closes on its next placement', () => {
  const closes = [];
  function Leaving() {
    const [anchor, setAnchor] = useState(null);
    const [drawn, setDrawn] = useState(true);
    return h(
      'div',
      null,
      drawn && h('button', { ref: (el) => el && setAnchor(el), type: 'button' }, 'Open'),
      h('button', { className: 'drop', onClick: () => setDrawn(false), type: 'button' }, 'Drop'),
      h(Popover, { anchor, id: 'pop', label: 'Box', onClose: () => closes.push('close'), open: true }, h('button', { type: 'button' }, 'In')),
    );
  }
  const { container } = render(h(Leaving));
  click(container.querySelector('.drop'));
  assert.deepEqual(closes, [], 'nothing places the box until the window moves');
  act(() => window.dispatchEvent(new window.Event('resize')));
  assert.deepEqual(closes, ['close'], 'the placement closes a box that hangs from nothing');
});

test('a box opened from inside an open dialog portals into the dialog', () => {
  function InDialog() {
    const [anchor, setAnchor] = useState(null);
    return h('dialog', { open: true }, h('button', { ref: setAnchor, type: 'button' }, 'Open'), h(Popover, { anchor, id: 'pop', label: 'Box', onClose() {}, open: true }, h('button', { type: 'button' }, 'In')));
  }
  render(h(InDialog));
  assert.ok(box()?.parentElement === document.querySelector('dialog'), 'the box stands inside the dialog, which leaves the rest of the document inert');
});

test('a new placeKey places the box again for the content it now holds', () => {
  const page = render(h(Opened, { onClose() {}, placeKey: 'list', rectOf: '40,600,96,30' }, content()));
  assert.equal(box().style.top, '638px', 'a short box stands under its anchor');
  boxSize = { height: 300, width: 200 };
  page.render(h(Opened, { onClose() {}, placeKey: 'values', rectOf: '40,600,96,30' }, content()));
  assert.equal(box().style.top, '292px', 'the taller step flips above the anchor');
});

test('a filter chip draws its word and values spans inside the body under the filter class, the x after them', () => {
  const opened = [];
  const removed = [];
  const chip = { field: 'models', label: 'model', mono: true, text: 'model: claude-opus-5, claude-sonnet-5', title: 'claude-opus-5, claude-sonnet-5', values: 'claude-opus-5, claude-sonnet-5' };
  const { container } = render(h(FilterChip, { chip, onOpen: (anchor) => opened.push(anchor), onRemove: () => removed.push('x') }));
  const root = container.firstElementChild;
  assert.deepEqual([root.tagName, root.className, root.title], ['SPAN', 'chip chip--filter chip--removable', 'claude-opus-5, claude-sonnet-5']);
  const [body, x] = root.children;
  assert.deepEqual([body.tagName, body.className, body.getAttribute('type')], ['BUTTON', 'chip__body', 'button']);
  assert.deepEqual([...body.children].map((el) => [el.className, el.textContent]), [['chip__word', 'model:'], ['chip__values mono', 'claude-opus-5, claude-sonnet-5']]);
  assert.equal(body.textContent, 'model: claude-opus-5, claude-sonnet-5', "the body reads the chip's whole text");
  assert.deepEqual([x.tagName, x.getAttribute('aria-label'), x.querySelector('svg')?.getAttribute('width')], ['BUTTON', 'Remove model filter', '12']);
  click(body);
  click(x);
  assert.ok(opened[0] === body && removed.length === 1, 'the body opens the chip anchored to itself, the x removes it');
  act(() => roots.pop().unmount());
  const flag = render(h(FilterChip, { chip: { field: 'held', label: 'held only', mono: false, text: 'held only', title: 'held only', values: null }, onOpen() {}, onRemove() {} }));
  assert.deepEqual([...flag.container.querySelector('.chip__body').children].map((el) => [el.className, el.textContent]), [['chip__word', 'held only']], 'a flag draws its word alone');
  act(() => roots.pop().unmount());
  const kinds = render(h(FilterChip, { chip: { field: 'kinds', label: 'kinds', mono: false, text: 'moved, held, placed +5', title: 'moved, held, placed, ...', values: 'moved, held, placed +5' }, onOpen() {}, onRemove() {} }));
  assert.deepEqual([...kinds.container.querySelector('.chip__body').children].map((el) => [el.className, el.textContent]), [['chip__word', 'kinds:'], ['chip__values', 'moved, held, placed +5']], "the values come from the chip, whatever its text reads");
});

const chipOf = (word) => ({ field: word, label: word, mono: false, text: `${word}: x`, title: 'x', values: 'x' });
const SEVEN = ['account', 'key', 'model', 'pool', 'status', 'effort', 'speed'].map(chipOf);
const lineOf = (props) => h(ChipLine, { addFilterRef: { current: null }, address: 'a', onClearAll() {}, onOpen() {}, onRemove() {}, ...props });
const shownWords = (container) => [...container.querySelectorAll('.chip--filter')].filter((el) => !el.hidden).map((el) => el.querySelector('.chip__word').textContent);

test('the chip line draws nothing without chips, five chips then a dashed +N more chip titled with the folded words, and Clear all from two chips on', () => {
  const none = render(lineOf({ chips: [] }));
  assert.equal(none.container.innerHTML, '', 'no chip, no line');
  none.render(lineOf({ chips: SEVEN.slice(0, 1) }));
  assert.deepEqual([shownWords(none.container), none.container.querySelector('.link-btn')], [['account:'], null], 'one chip and no Clear all');
  const cleared = [];
  none.render(lineOf({ chips: SEVEN.slice(0, 2), onClearAll: () => cleared.push('all') }));
  const clear = none.container.querySelector('.chip-line > .link-btn');
  assert.equal(clear?.textContent, 'Clear all', 'two chips draw Clear all');
  click(clear);
  assert.deepEqual(cleared, ['all']);
  none.render(lineOf({ chips: SEVEN }));
  const line = none.container.querySelector('.chip-line');
  assert.deepEqual(shownWords(none.container), ['account:', 'key:', 'model:', 'pool:', 'status:'], 'a line no layout measured holds five chips');
  const more = line.querySelector('.chip--more');
  assert.deepEqual([more?.tagName, more?.className, more?.textContent, more?.title, more?.getAttribute('aria-expanded')], ['BUTTON', 'chip chip--more', '+2 more', 'Folded: effort, speed', 'false']);
  assert.ok(line.lastElementChild === line.querySelector('.link-btn') && more.nextElementSibling === line.lastElementChild, 'Clear all ends the line after the fold');
});

test('+N more shows every chip with Show fewer at the end, and a new address folds the line again, a return to the old one too', () => {
  const page = render(lineOf({ chips: SEVEN }));
  const more = page.container.querySelector('.chip--more');
  focus(more);
  click(more);
  const line = page.container.querySelector('.chip-line');
  assert.deepEqual([shownWords(page.container).length, line.className, more.textContent, more.getAttribute('aria-expanded')], [7, 'chip-line is-expanded', 'Show fewer', 'true']);
  assert.ok(document.activeElement === more && more.previousElementSibling.querySelector('.chip__word').textContent === 'speed:', 'the same button, now Show fewer, keeps focus after the last chip');
  click(more);
  assert.deepEqual([shownWords(page.container).length, line.className, more.textContent], [5, 'chip-line', '+2 more'], 'Show fewer folds them again');
  click(more);
  page.render(lineOf({ address: 'b', chips: SEVEN }));
  assert.deepEqual([shownWords(page.container).length, page.container.querySelector('.chip--more').textContent], [5, '+2 more'], 'a new address folds the line with the same chips');
  page.render(lineOf({ address: 'a', chips: SEVEN }));
  assert.deepEqual([shownWords(page.container).length, page.container.querySelector('.chip-line').className], [5, 'chip-line'], 'a return to the address it was expanded on stays folded');
});

test("a removed chip hands focus to the next chip's body, the last one to Add filter, and Clear all to Add filter", () => {
  const addRef = { current: null };
  function Removing({ initial }) {
    const [chips, setChips] = useState(initial);
    return h(
      'div',
      null,
      h('button', { className: 'add', ref: addRef, type: 'button' }, 'Add filter'),
      h(ChipLine, { addFilterRef: addRef, address: chips.map((c) => c.field).join(','), chips, onClearAll: () => setChips([]), onOpen() {}, onRemove: (field) => setChips((all) => all.filter((c) => c.field !== field)) }),
    );
  }
  const page = render(h(Removing, { initial: SEVEN.slice(0, 3) }));
  const x = (word) => page.container.querySelector(`[aria-label="Remove ${word} filter"]`);
  focus(x('key'));
  click(x('key'));
  assert.equal(document.activeElement?.closest('.chip__body')?.textContent, 'model: x', "focus moves to the next chip's body");
  focus(x('model'));
  click(x('model'));
  assert.ok(document.activeElement === addRef.current, 'the last chip hands focus to Add filter');
  act(() => roots.pop().unmount());
  const again = render(h(Removing, { initial: SEVEN.slice(0, 3) }));
  const clear = again.container.querySelector('.chip-line > .link-btn');
  focus(clear);
  click(clear);
  assert.ok(again.container.querySelector('.chip-line') === null && document.activeElement === addRef.current, 'Clear all takes the line away and hands focus to Add filter');
});

const DIMENSIONS = [
  { field: 'accounts', group: 'Routing', kind: 'list', label: 'Account', word: 'account' },
  { field: 'keys', group: 'Routing', kind: 'list', label: 'Key', mono: true, word: 'key' },
  { field: 'held', group: 'Outcome', kind: 'flag', label: 'Held only', word: 'held only' },
  { anyLabel: 'Any kind', field: 'kind', group: 'Request', kind: 'choice', label: 'Kind', word: 'kind' },
  { field: 'agent', group: 'Request', kind: 'pattern', label: 'User agent', mono: true, word: 'user agent' },
  { field: 'session', group: 'Subject', kind: 'id', label: 'Session', mono: true, word: 'session' },
];
const OPTIONS = {
  accounts: [
    { label: 'primary', value: 'acc_1' },
    { label: 'nightly', value: 'acc_2' },
  ],
  keys: [],
  kind: [
    { label: 'Inference', value: 'inference' },
    { label: 'Probe', value: 'probe' },
  ],
};
const EMPTY = { accounts: [], agent: '', held: false, keys: [], kind: '', session: '' };

/** A bar's wiring: the filters in state, Add filter and the chip line over them, a chip's click opening its step. */
function bar(initial, { dimensions = DIMENSIONS, disabled, options = OPTIONS } = {}) {
  const addRef = { current: null };
  const writes = [];
  function Bar() {
    const [filters, setFilters] = useState({ ...EMPTY, ...initial });
    const [edit, setEdit] = useState(null);
    const update = (patch) => {
      writes.push(patch);
      setFilters((f) => ({ ...f, ...patch }));
    };
    const chips = chipsOf(dimensions, filters, options);
    return h(
      'div',
      null,
      h(AddFilter, { buttonRef: addRef, dimensions, disabled, edit, filters, onEditDone: () => setEdit(null), options, update }),
      h(ChipLine, {
        addFilterRef: addRef,
        address: JSON.stringify(filters),
        chips,
        onClearAll() {},
        onOpen: (field, anchor) => setEdit({ anchor, field }),
        onRemove: (field) => update({ [field]: emptyValue(dimensions.find((x) => x.field === field)) }),
      }),
    );
  }
  const page = render(h(Bar));
  const button = () => addRef.current;
  const rows = () => [...box().querySelectorAll('.add-filter__row')];
  const rowOf = (label) => rows().find((el) => el.firstElementChild?.textContent === label);
  const chipBody = (word) => [...page.container.querySelectorAll('.chip__body')].find((el) => el.textContent.startsWith(`${word}:`));
  return { ...page, button, chipBody, rowOf, rows, writes };
}

test('Add filter opens step one with the Filter by field focused, the groups and their rows with each value, and a disabled row the keys skip', () => {
  const b = bar({ accounts: ['acc_1'], held: true }, { disabled: { keys: 'The key list did not load: offline' } });
  const button = b.button();
  assert.deepEqual([button.className.includes('btn--sm'), button.textContent, button.querySelector('svg')?.getAttribute('width'), button.getAttribute('aria-haspopup'), button.getAttribute('aria-expanded')], [true, 'Add filter', '14', 'dialog', 'false']);
  click(button);
  const el = box();
  assert.deepEqual([button.getAttribute('aria-expanded'), el?.getAttribute('aria-label'), el?.getAttribute('aria-labelledby')], ['true', 'Add filter', null]);
  const field = el.querySelector('.add-filter__search');
  assert.ok(document.activeElement === field && field.getAttribute('aria-label') === 'Filter by', 'the Filter by field has focus');
  assert.deepEqual([...el.querySelectorAll('.mselect__group')].map((g) => g.textContent), ['Routing', 'Outcome', 'Request', 'Subject']);
  assert.deepEqual(
    b.rows().map((r) => [r.firstElementChild.textContent, r.querySelector('.add-filter__value')?.textContent ?? '']),
    [['Account', 'primary'], ['Key', ''], ['Held only', 'on'], ['Kind', ''], ['User agent', ''], ['Session', '']],
    'each row shows its current value in the chip words without the dimension',
  );
  const key = b.rowOf('Key');
  assert.deepEqual([key.getAttribute('aria-disabled'), key.title], ['true', 'The key list did not load: offline']);
  const walk = ['ArrowDown', 'ArrowDown', 'ArrowUp', 'ArrowUp'].map((k) => {
    press(k);
    return document.activeElement === field ? 'field' : document.activeElement.firstElementChild?.textContent;
  });
  assert.deepEqual(walk, ['Account', 'Held only', 'Account', 'field'], 'the keys walk the field and the rows past the disabled one');
  click(key);
  assert.ok(box().querySelector('.add-filter__search'), 'a click on the disabled row opens nothing');
  type(field, 'us');
  assert.deepEqual([b.rows().map((r) => r.firstElementChild.textContent), [...box().querySelectorAll('.mselect__group')].map((g) => g.textContent)], [['User agent'], ['Request']], 'the field filters the rows by label and drops the empty groups');
});

test('a list row opens step two on its option list, applying on each click, and Back returns to step one with the field focused', () => {
  const b = bar({ accounts: ['acc_1'] });
  click(b.button());
  click(b.rowOf('Account'));
  const el = box();
  const title = el.querySelector('.popover__head .popover__title');
  assert.deepEqual([title?.textContent, el.getAttribute('aria-labelledby') === title?.id, el.querySelector('.popover__head [aria-label="Back to the filter list"]') !== null, el.querySelector('.popover__head .link-btn')?.textContent], ['Account', true, true, 'Remove']);
  assert.ok(document.activeElement === el.querySelector('.option-list [role="option"]'), 'the option list takes focus on its first row');
  click([...el.querySelectorAll('.mselect__option')].find((o) => o.textContent === 'nightly'));
  assert.deepEqual(b.writes, [{ accounts: ['acc_1', 'acc_2'] }]);
  assert.ok(box() !== null && b.chipBody('account')?.textContent === 'account: primary, nightly', 'the box stays open and the chip reads both');
  click(el.querySelector('[aria-label="Back to the filter list"]'));
  assert.ok(document.activeElement === box().querySelector('.add-filter__search'), 'Back focuses the Filter by field');
});

test('a pattern commits on Enter or Apply and drops its draft on Escape, and focus returns to Add filter', () => {
  const b = bar({});
  click(b.button());
  click(b.rowOf('User agent'));
  const field = box().querySelector('input');
  assert.ok(document.activeElement === field && field.title === PATTERN_GRAMMAR && field.classList.contains('mono'), 'the mono field with the grammar in its title has focus');
  type(field, 'claude-cli/*');
  press('Escape');
  assert.deepEqual([box(), b.writes, document.activeElement === b.button()], [null, [], true], 'Escape closes, writes nothing and refocuses Add filter');
  click(b.button());
  click(b.rowOf('User agent'));
  type(box().querySelector('input'), ' claude-cli/2.1.* ');
  press('Enter');
  assert.deepEqual([box(), b.writes, document.activeElement === b.button()], [null, [{ agent: 'claude-cli/2.1.*' }], true], 'Enter commits the trimmed pattern and closes');
  click(b.button());
  click(b.rowOf('Session'));
  type(box().querySelector('input'), 'd7167642-3f1a-4c2e-9b8d-5e6f7a8b9c0d');
  click(box().querySelector('.popover__foot .btn--primary'));
  assert.deepEqual([box(), b.writes.at(-1), document.activeElement === b.button(), b.chipBody('session')?.textContent], [null, { session: 'd7167642-3f1a-4c2e-9b8d-5e6f7a8b9c0d' }, true, 'session: d7167642…'], 'Apply commits the id');
});

test('a choice applies and closes on a click with the any row first, and a flag row becomes a chip at once', () => {
  const b = bar({});
  click(b.button());
  click(b.rowOf('Kind'));
  const radios = [...box().querySelectorAll('[role="radiogroup"] [role="radio"]')];
  assert.deepEqual(radios.map((r) => [r.textContent, r.getAttribute('aria-checked')]), [['Any kind', 'true'], ['Inference', 'false'], ['Probe', 'false']]);
  assert.ok(document.activeElement === radios[0], 'the first choice row has focus');
  click(radios[1]);
  assert.deepEqual([box(), b.writes, document.activeElement === b.button(), b.chipBody('kind')?.textContent], [null, [{ kind: 'inference' }], true, 'kind: Inference']);
  click(b.button());
  click(b.rowOf('Held only'));
  assert.deepEqual([box(), b.writes.at(-1), document.activeElement === b.button()], [null, { held: true }, true], 'the flag writes at once and closes');
  assert.ok([...b.container.querySelectorAll('.chip__body')].some((el) => el.textContent === 'held only'));
});

test("a chip's click opens step two anchored at the chip, and Remove clears the dimension, closes and moves focus to the next chip or to Add filter", () => {
  const b = bar({ accounts: ['acc_1'], agent: 'claude*', kind: 'probe' });
  const kindChip = b.chipBody('kind');
  click(kindChip);
  assert.deepEqual([box()?.querySelector('.popover__title')?.textContent, b.button().getAttribute('aria-expanded')], ['Kind', 'false'], 'the chip opens its own step, not Add filter');
  assert.ok(document.activeElement === box().querySelector('[role="radio"]'), 'the first choice row has focus');
  click(box().querySelector('.popover__head .link-btn'));
  assert.deepEqual([box(), b.writes.at(-1)], [null, { kind: '' }]);
  assert.ok(document.activeElement === b.chipBody('user agent'), "focus moves to the next chip's body");
  click(b.chipBody('user agent'));
  assert.equal(document.activeElement?.value, 'claude*', 'the pattern field opens on the value');
  click(box().querySelector('.popover__head .link-btn'));
  assert.deepEqual([box(), b.writes.at(-1), document.activeElement === b.button()], [null, { agent: '' }, true], 'the last chip hands focus to Add filter');
  click(b.chipBody('account'));
  click([...box().querySelectorAll('.mselect__option')].find((o) => o.textContent === 'primary'));
  assert.deepEqual([b.writes.at(-1), b.chipBody('account'), box() !== null, box().contains(document.activeElement)], [{ accounts: [] }, undefined, true, true], 'emptying the list from its chip keeps the box open with the focus in it');
  press('Escape');
  assert.ok(box() === null && document.activeElement === b.button(), 'the box hangs from Add filter once its chip has left');
});

const XY = [
  { label: 'x', value: 'x' },
  { label: 'y', value: 'y' },
];
const listOf = (field) => ({ field, group: 'G', kind: 'list', label: field.toUpperCase(), word: field });
const KIND = { anyLabel: 'Any kind', field: 'kind', group: 'G', kind: 'choice', label: 'Kind', word: 'kind' };
/** Seven chips on one line, the seventh `last`, expanded so the seventh can be clicked. */
function expandedBar(last) {
  const dimensions = [...['a', 'b', 'c', 'd', 'e'].map(listOf), ...(last === KIND ? [listOf('f'), KIND] : [KIND, last])];
  const options = { ...Object.fromEntries(['a', 'b', 'c', 'd', 'e', 'f', 'g'].map((f) => [f, XY])), kind: OPTIONS.kind };
  const b = bar({ a: ['x'], b: ['x'], c: ['x'], d: ['x'], e: ['x'], f: ['x'], g: ['x'], kind: 'probe' }, { dimensions, options });
  click(b.container.querySelector('.chip--more'));
  assert.equal(b.container.querySelector('.chip-line').className, 'chip-line is-expanded');
  return b;
}

test("a choice made from the expanded line's seventh chip folds the line and hands the focus to the fold chip that now holds it", () => {
  const b = expandedBar(KIND);
  click(b.chipBody('kind'));
  click([...box().querySelectorAll('[role="radio"]')].find((r) => r.textContent === 'Inference'));
  const more = b.container.querySelector('.chip--more');
  assert.deepEqual([box(), b.writes.at(-1), b.container.querySelector('.chip-line').className, b.chipBody('kind')], [null, { kind: 'inference' }, 'chip-line', undefined], 'the write folds the line and the chip with it');
  assert.ok(document.activeElement === more && more.textContent === '+2 more', 'the fold chip takes the focus');
});

test("a list edited from the expanded line's seventh chip moves the box to Add filter as the line folds, with the focus kept in it", () => {
  const b = expandedBar(listOf('g'));
  click(b.chipBody('g'));
  assert.equal(box()?.querySelector('.popover__title')?.textContent, 'G');
  click([...box().querySelectorAll('.mselect__option')].find((o) => o.textContent === 'y'));
  assert.deepEqual([b.writes.at(-1), b.chipBody('g'), box() !== null, box().contains(document.activeElement), b.button().getAttribute('aria-expanded')], [{ g: ['x', 'y'] }, undefined, true, true, 'true'], 'the box stays open on Add filter with the focus in it');
  press('Escape');
  assert.ok(box() === null && document.activeElement === b.button(), 'Escape closes it onto Add filter');
});

test('a list step with no option to show keeps the focus in the box on Back, so Escape still closes it', () => {
  const b = bar({});
  click(b.button());
  click(b.rowOf('Key'));
  const back = box()?.querySelector('[aria-label="Back to the filter list"]');
  assert.deepEqual([box()?.querySelector('.mselect__empty')?.textContent, document.activeElement === back], ['No matches', true], 'the empty list leaves the focus on Back');
  press('Escape');
  assert.ok(box() === null && document.activeElement === b.button(), 'Escape closes the box onto Add filter');
});

test('a list with one group draws no group heading', () => {
  const b = bar({}, { dimensions: KEYS_DIMENSIONS });
  click(b.button());
  assert.deepEqual([box().querySelectorAll('.mselect__group').length, b.rows().map((r) => r.firstElementChild.textContent)], [0, ['Pool', 'Status']]);
});
