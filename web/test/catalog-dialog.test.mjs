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
const { act, api, CatalogDialog, createElement: h, createRoot } = await import('./dist/ui.js');

const roots = [];
const { getAccountCatalog } = api;
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
  api.getAccountCatalog = getAccountCatalog;
});

// A read answers on a promise, which settles after a synchronous act has returned, so the wait steps out of the act
// environment for a few scheduler turns.
async function settled() {
  globalThis.IS_REACT_ACT_ENVIRONMENT = false;
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setImmediate(resolve));
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
}

const model = { createdAt: '2026-08-01T00:00:00Z', displayName: 'Test model', effortLevels: [], id: 'test-model', maxInputTokens: null, maxTokens: null };
const at = Date.UTC(2026, 9, 14, 12);

test('a failed Refresh keeps the list and shows the failure, and the next success clears it', async () => {
  // The opening's read answers the list, the first Refresh fails and the second answers again.
  const answers = [
    () => Promise.resolve({ error: null, fetchedAt: new Date(at).toISOString(), models: [model] }),
    () => Promise.reject(new Error('the upstream did not answer')),
    () => Promise.resolve({ error: null, fetchedAt: new Date(at).toISOString(), models: [model] }),
  ];
  api.getAccountCatalog = () => answers.shift()();
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(h(CatalogDialog, { accountId: 'acct', label: 'Account', now: at, onClose() {}, open: true })));
  await settled();
  const read = () => ({
    failure: container.querySelector('dialog[open] [role="alert"]')?.textContent ?? null,
    models: [...container.querySelectorAll('dialog[open] tbody tr .catalog__id')].map((td) => td.textContent),
  });
  const refresh = async () => {
    act(() => [...container.querySelectorAll('dialog[open] button')].find((b) => b.textContent?.trim() === 'Refresh')?.click());
    await settled();
  };
  assert.deepEqual(read(), { failure: null, models: ['test-model'] }, 'the opening draws the list');
  await refresh();
  assert.deepEqual(read(), { failure: 'the upstream did not answer', models: ['test-model'] }, 'a failed Refresh draws the failure over the list it kept');
  await refresh();
  assert.deepEqual(read(), { failure: null, models: ['test-model'] }, 'the next answer clears the failure');
});
