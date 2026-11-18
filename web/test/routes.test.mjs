import assert from 'node:assert/strict';
import { after, afterEach, test } from 'node:test';
import { JSDOM, VirtualConsole } from 'jsdom';

// jsdom answers a reload with a "not implemented" error on its console, which is how a case tells the tab was asked
// to reload.
const navigations = [];
const virtualConsole = new VirtualConsole();
virtualConsole.sendTo(console, { omitJSDOMErrors: true });
virtualConsole.on('jsdomError', (error) => navigations.push(error.message));
// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true, url: 'http://127.0.0.1/pools', virtualConsole });
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
after(() => window.close());
const { act, api, createElement: h, createMemoryRouter, createRoot, lazy, MeterPage, Outlet, PAGE_TITLES, PageLoadFailed, RouterProvider, Suspense } = await import('./dist/ui.js');
const { reloadOnChunkFailure } = await import('./dist/api.js');

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
});

// A chunk's load settles on a promise after a synchronous act has returned, so the wait steps out of the act
// environment for a few scheduler turns.
async function settled() {
  globalThis.IS_REACT_ACT_ENVIRONMENT = false;
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setImmediate(resolve));
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
}

/** A tab's session storage and its reload, recorded, kept across the two loads as a tab keeps them. */
const tab = () => {
  const items = new Map();
  const env = {
    reloads: 0,
    reload: () => {
      env.reloads += 1;
    },
    storage: () => ({ getItem: (k) => items.get(k) ?? null, removeItem: (k) => items.delete(k), setItem: (k, v) => items.set(k, String(v)) }),
  };
  return { env, items };
};
const missing = () => Promise.reject(new TypeError('Failed to fetch dynamically imported module'));

/**
 * A render that throws to the error element, with `console.error` stubbed while it runs: React and React Router print
 * the caught error with its stack, which would bury the run's own output. The stub goes whatever the render does.
 */
async function quietly(render) {
  const error = console.error;
  console.error = () => {};
  try {
    return await render();
  } finally {
    console.error = error;
  }
}

/**
 * The console's routes in their shape: a page loaded through React's `lazy` over the chunk reload, inside its
 * Suspense, on a child route that carries the error element, under a parent that draws its outlet. The parent stands
 * in for the shell, which needs the session and the settings providers. The smoke run holds that the console's own
 * lazy routes carry the element. A case that throws something else passes its own page.
 */
async function load(env, Page = lazy(reloadOnChunkFailure(missing, env))) {
  const router = createMemoryRouter(
    [
      {
        path: '/',
        element: h('div', { 'data-shell': '' }, h('nav', null, 'Pools'), h(Outlet)),
        children: [{ path: 'pools', element: h(Suspense, { fallback: h('p', { className: 'fallback' }, 'Loading') }, h(Page)), errorElement: h(PageLoadFailed) }],
      },
    ],
    { initialEntries: ['/pools'] },
  );
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(h(RouterProvider, { router })));
  await settled();
  return container;
}

test('a page chunk that fails again after the reload draws its error inside the shell, with a Reload button', async () => {
  const { env, items } = tab();
  const first = await load(env);
  assert.deepEqual([env.reloads, items.size, first.querySelector('[data-shell] .fallback')?.textContent ?? null], [1, 1, 'Loading'], 'the first failure reloads the tab once and leaves the page loading');
  // The reloaded tab builds its routes anew over the same storage, so the second failure throws to the element.
  const second = await quietly(() => load(env));
  const shell = second.querySelector('[data-shell]');
  const failed = shell?.querySelector('.empty--error[role="alert"]');
  assert.deepEqual(
    [shell?.querySelector('nav')?.textContent ?? null, failed?.querySelector('.empty__title')?.textContent ?? null, failed?.querySelector('.empty__body')?.textContent ?? null, failed?.querySelector('button')?.textContent ?? null],
    ['Pools', 'This page could not load', "The console's files changed under this tab, and a reload did not fetch the current ones. Reload to try again.", 'Reload'],
    "the element draws in the shell's outlet, the shell around it",
  );
  assert.equal(env.reloads, 1, 'the second failure reloads nothing on its own');
  navigations.splice(0);
  act(() => failed?.querySelector('button')?.click());
  assert.deepEqual(navigations.map((m) => m.startsWith('Not implemented: navigation')), [true], 'Reload asks the tab to reload');
});

const outlet = (container) => {
  const shell = container.querySelector('[data-shell]');
  const failed = shell?.querySelector('.empty--error[role="alert"]');
  return [shell?.querySelector('nav')?.textContent ?? null, failed?.querySelector('.empty__title')?.textContent ?? null, failed?.querySelector('.empty__body')?.textContent ?? null, failed?.querySelector('button')?.textContent ?? null];
};

test('a page that throws while drawing reads as an error of its own inside the shell, with the same Reload button', async () => {
  const { env } = tab();
  const Broken = () => {
    throw new Error('boom');
  };
  const drawn = await quietly(() => load(env, lazy(reloadOnChunkFailure(() => Promise.resolve({ default: Broken }), env))));
  assert.deepEqual(
    outlet(drawn),
    ['Pools', 'This page hit an error', 'Something went wrong while drawing this page. Reload to try again, and if it comes back, note what you opened last.', 'Reload'],
    'a render error names no bundle that changed under the tab',
  );
  assert.equal(env.reloads, 0, 'a render error reloads nothing on its own');
  navigations.splice(0);
  act(() => drawn.querySelector('[data-shell] .empty--error button')?.click());
  assert.deepEqual(navigations.map((m) => m.startsWith('Not implemented: navigation')), [true], 'Reload asks the tab to reload');
});

test("a page chunk that fails with WebKit's import error reads as a failed chunk too", async () => {
  const { env } = tab();
  const webkit = () => Promise.reject(new TypeError('Importing a module script failed.'));
  await load(env, lazy(reloadOnChunkFailure(webkit, env)));
  const second = await quietly(() => load(env, lazy(reloadOnChunkFailure(webkit, env))));
  assert.deepEqual(outlet(second), ['Pools', 'This page could not load', "The console's files changed under this tab, and a reload did not fetch the current ones. Reload to try again.", 'Reload']);
  assert.equal(env.reloads, 1, 'the first failure reloads the tab once and the second throws to the element');
});

test('the Meter view draws at /meter with its two cards, its title named for the shell', async () => {
  // The answers are the case's own, so the page reads no router: one completed 5-hour window and one 7-day window.
  const end = Date.parse('2026-09-16T13:11:00Z');
  const window = (name, observedRise, predictedShare) => ({ accountId: 'acct-a', computedAt: new Date(end + 2_220_000).toISOString(), estimated: 0, firstReadingAt: new Date(end - 3_600_000).toISOString(), lastReadingAt: new Date(end - 60_000).toISOString(), observedRise, predictedShare, ratio: observedRise / predictedShare, requests: 12, window: name, windowEnd: new Date(end).toISOString() });
  const { getAnalyticsReconciliations, listAccounts } = api;
  api.getAnalyticsReconciliations = async () => ({ reconciliations: [window('five_hour', 0.3, 0.3), window('seven_day', 0.5, 0.5)], requestRetentionDays: 30 });
  api.listAccounts = async () => [{ id: 'acct-a', label: 'acct-a' }];
  try {
    const router = createMemoryRouter([{ path: '/', element: h('div', { 'data-shell': '' }, h(Outlet)), children: [{ path: 'meter', element: h(MeterPage) }] }], { initialEntries: ['/meter'] });
    const container = document.body.appendChild(document.createElement('div'));
    const root = createRoot(container);
    roots.push(root);
    act(() => root.render(h(RouterProvider, { router })));
    await settled();
    assert.deepEqual([...container.querySelectorAll('[data-shell] .card__title')].map((t) => t.textContent), ['Weekly windows', '5-hour windows'], 'the weekly card above the 5-hour one');
    assert.equal(PAGE_TITLES['/meter'], 'Meter', "the shell's title for the path");
  } finally {
    Object.assign(api, { getAnalyticsReconciliations, listAccounts });
  }
});

test('a Meter read that fails with an empty message draws the error row in each card', async () => {
  // A refusal body whose error is empty reaches the page as a failure with no text.
  const { getAnalyticsReconciliations, listAccounts } = api;
  api.getAnalyticsReconciliations = async () => {
    throw new Error('');
  };
  api.listAccounts = async () => [{ id: 'acct-a', label: 'acct-a' }];
  try {
    const router = createMemoryRouter([{ path: '/', element: h('div', { 'data-shell': '' }, h(Outlet)), children: [{ path: 'meter', element: h(MeterPage) }] }], { initialEntries: ['/meter'] });
    const container = document.body.appendChild(document.createElement('div'));
    const root = createRoot(container);
    roots.push(root);
    act(() => root.render(h(RouterProvider, { router })));
    await settled();
    assert.deepEqual(
      [...container.querySelectorAll('[data-shell] .card--table')].map((card) => [card.querySelector('.empty--error .empty__title')?.textContent ?? null, card.querySelector('td.muted')?.textContent ?? null]),
      [
        ['Could not load the meter reconciliation', null],
        ['Could not load the meter reconciliation', null],
      ],
      'a failure with no text still reads as a failure, so neither card draws its empty row',
    );
  } finally {
    Object.assign(api, { getAnalyticsReconciliations, listAccounts });
  }
});
