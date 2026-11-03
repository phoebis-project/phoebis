import assert from 'node:assert/strict';
import { after, afterEach, beforeEach, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
// The chart ink reads the theme on an animation frame, so the frame calls stand beside the DOM.
for (const key of ['document', 'navigator', 'HTMLElement', 'HTMLDialogElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event', 'requestAnimationFrame', 'cancelAnimationFrame']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
// The settings provider follows the color scheme through matchMedia, which jsdom lacks, so the shim answers as the
// smoke run's does.
window.matchMedia = () => ({ matches: false, addEventListener() {}, removeEventListener() {} });
// Node 26 carries a `localStorage` global of its own, which the bundle would read in place of the window's, so the
// getter hands it the window's storage.
Object.defineProperty(globalThis, 'localStorage', { configurable: true, get: () => window.localStorage });
// jsdom has no showModal, so the shims keep the open attribute in step with the calls, as the smoke run's do.
window.HTMLDialogElement.prototype.showModal = function () {
  this.setAttribute('open', '');
};
window.HTMLDialogElement.prototype.close = function () {
  this.removeAttribute('open');
  this.dispatchEvent(new window.Event('close'));
};
after(() => window.close());
const { AccountsPage, act, api, AppSettingsProvider, createElement: h, createMemoryRouter, createRoot, fakeClock, installClock, RouterProvider, SessionProvider } = await import('./dist/ui.js');
const { MockApi } = await import('./dist/api.js');

// A subscription account that draws the allowance card, as the input shape the page lists. No figure of it is read.
const account = (await new MockApi().listAccounts()).find((a) => a.kind === 'oauth' && !a.usageBased);
assert.ok(account, 'the mock holds a subscription account that is no usage-based seat');

const roots = [];
const reads = { getAnalyticsReconciliations: api.getAnalyticsReconciliations, getAnalyticsUnusedAllowance: api.getAnalyticsUnusedAllowance, getQuotaHistory: api.getQuotaHistory, getSession: api.getSession, getSettings: api.getSettings, listAccounts: api.listAccounts };
beforeEach(() => {
  // A frozen clock, so the page's poll and its ticking instant arm no window timer.
  const clock = fakeClock();
  clock.freeze(Date.UTC(2026, 9, 14, 12));
  installClock(clock);
  // The test build's api is the HTTP client, so every read the page and the shell's session make answers here.
  api.getSession = async () => ({ persistent: false, sessionId: 'session' });
  api.listAccounts = async () => [account];
  api.getQuotaHistory = async () => [];
  api.getAnalyticsReconciliations = async () => ({ reconciliations: [], requestRetentionDays: 30 });
  // The route lists no reset for the account, so the card draws its sentence for that once the cost model stands.
  api.getAnalyticsUnusedAllowance = async () => ({ accounts: [] });
});
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
  Object.assign(api, reads);
  installClock(null);
});

// A read answers on a promise, which settles after a synchronous act has returned, so the wait steps out of the act
// environment for a few scheduler turns.
async function settled() {
  globalThis.IS_REACT_ACT_ENVIRONMENT = false;
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setImmediate(resolve));
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
}

/** The Accounts view on `account`'s detail, under the providers the shell mounts it in. */
async function mountDetail() {
  const router = createMemoryRouter([{ path: '/accounts/:accountId', element: h(AccountsPage) }], { initialEntries: [`/accounts/${account.id}`] });
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  roots.push(root);
  act(() => root.render(h(SessionProvider, null, h(AppSettingsProvider, null, h(RouterProvider, { router })))));
  await settled();
  return container;
}

const allowanceCard = (container) => [...container.querySelectorAll('.card')].find((c) => c.querySelector('.card__title')?.textContent?.trim() === 'Allowance left at reset');

test('a settings read that fails once, then Try again, draws the allowance card from the second answer', async () => {
  let settingsReads = 0;
  api.getSettings = async () => {
    settingsReads += 1;
    if (settingsReads === 1) throw new Error('the configuration store is unavailable');
    return { subscriptionCosts: { problem: null } };
  };
  const container = await mountDetail();
  const failed = allowanceCard(container);
  assert.deepEqual(
    [failed?.querySelector('.empty__title')?.textContent ?? null, failed?.querySelector('.empty__body')?.textContent ?? null],
    ['Could not load the weekly resets', 'the configuration store is unavailable'],
    'the failed settings read draws the card failed',
  );
  act(() => [...(failed?.querySelectorAll('button') ?? [])].find((b) => b.textContent?.trim() === 'Try again')?.click());
  await settled();
  const card = allowanceCard(container);
  assert.deepEqual(
    [card?.querySelector('.empty__title')?.textContent ?? null, card?.querySelector(':scope > p.muted')?.textContent ?? null],
    [null, "No weekly reset recorded yet. The first shows once this account's week ends."],
    "Try again reads the settings again, and the card draws from that answer's cost model",
  );
});
