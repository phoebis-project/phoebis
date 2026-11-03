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
const { act, createElement: h, createMemoryRouter, createRoot, RouterProvider, useQueryFilters } = await import('./dist/ui.js');
const { codecOf } = await import('./dist/api.js');

// The case's own address: two lists, a range, a text field and the unit a link may carry.
const CODEC = codecOf({ accounts: { kind: 'list', name: 'accounts' }, models: { kind: 'list', name: 'models' }, range: { default: '24h', kind: 'range' }, search: { kind: 'text', name: 'q' }, unit: { kind: 'text', name: 'unit' } });

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
});

// The router settles a navigation on a promise, after a synchronous act has returned, so the wait steps out of the act
// environment for a few scheduler turns.
async function settled() {
  globalThis.IS_REACT_ACT_ENVIRONMENT = false;
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setImmediate(resolve));
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
}

/** A view at `path` whose every render records the filters the hook answers, and the router to move its address. */
async function probe(path) {
  const seen = [];
  function Probe() {
    seen.push(useQueryFilters(CODEC).filters);
    return null;
  }
  const router = createMemoryRouter([{ path: '/view', element: h(Probe) }], { initialEntries: [path] });
  const root = createRoot(document.body.appendChild(document.createElement('div')));
  roots.push(root);
  act(() => root.render(h(RouterProvider, { router })));
  await settled();
  const go = async (to) => {
    act(() => void router.navigate(to));
    await settled();
  };
  return { go, last: () => seen.at(-1) };
}

test("an address change that moves only the unit keeps each filter field's identity", async () => {
  const view = await probe('/view?accounts=a,b&models=m&range=7d&q=s&unit=usd');
  const before = view.last();
  await view.go('/view?accounts=a,b&models=m&range=7d&q=s&unit=native');
  const now = view.last();
  assert.equal(now.unit, 'native', 'the unit reads the new address');
  assert.deepEqual(
    { accounts: now.accounts === before.accounts, models: now.models === before.models, range: now.range === before.range },
    { accounts: true, models: true, range: true },
    'each list and the range keep the identity they had',
  );
});

test("an address change that moves a list reads the list's new value", async () => {
  const view = await probe('/view?accounts=a,b&range=7d');
  await view.go('/view?accounts=a,c&range=7d');
  assert.deepEqual(view.last().accounts, ['a', 'c'], 'the moved list reads its new ids, never the one it held');
});
