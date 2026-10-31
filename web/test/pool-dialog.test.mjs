import assert from 'node:assert/strict';
import { after, afterEach, beforeEach, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'HTMLDialogElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
// The settings provider follows the color scheme through matchMedia, which jsdom lacks, so the shim answers as the
// smoke run's does.
window.matchMedia = () => ({ matches: false, addEventListener() {}, removeEventListener() {} });
// Node 26 carries a `localStorage` global of its own, which the bundle would read in place of the window's, so the
// getter hands it the window's storage. That storage throws on a window without an origin, so the provider's read of
// the expert view fails and the dialogs draw the plain view, as they do in a browser that blocks storage.
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
const { act, api, AppSettingsProvider, createElement: h, createRoot, fakeClock, installClock, KeyDialog, MemoryRouter, PoolDialog } = await import('./dist/ui.js');
const { MockApi } = await import('./dist/api.js');

// The bootstrap's limit the held settings read answers with, the test's own figure.
const LIMIT = 72;
const SETTINGS = { retention: { bodyCaptureMaxHours: LIMIT } };

const roots = [];
const reads = { getKeyModels: api.getKeyModels, getSettings: api.getSettings, listCatalogOf: api.listCatalogOf };
beforeEach(() => {
  // A frozen clock, so the dialog's ticking instant arms no window timer.
  const clock = fakeClock();
  clock.freeze(Date.UTC(2026, 9, 14, 12));
  installClock(clock);
  // The test build's api is the HTTP client, so the dialogs' other reads answer empty here: the members' catalogs
  // after the suggestions' pause, and an open edit's model list.
  api.listCatalogOf = async () => [];
  api.getKeyModels = async (keyId) => ({ keyId, longContextTwins: true, modelRulesMode: 'extend', poolId: '', rules: [] });
});
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
  Object.assign(api, reads);
  installClock(null);
});

/** The settings read held until `answer` lands it. */
function holdSettings() {
  let answer;
  const held = new Promise((resolve) => {
    answer = resolve;
  });
  api.getSettings = () => held;
  return () => answer(SETTINGS);
}

// A held read answers on a promise, which settles after a synchronous act has returned, so the wait steps out of the
// act environment for a few scheduler turns. An awaited act would keep the test process alive on its message channel.
async function settled() {
  globalThis.IS_REACT_ACT_ENVIRONMENT = false;
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setImmediate(resolve));
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
}

/**
 * A root that draws `node` under the router and the settings provider the pages mount inside. Each draw waits out the
 * reads that answer at once, so they land before the case reads the dialog.
 */
function mount() {
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  roots.push(root);
  const draw = async (node) => {
    act(() => root.render(h(MemoryRouter, null, h(AppSettingsProvider, null, node))));
    await settled();
  };
  return { container, draw };
}

// React tracks a field's value through the prototype's setter, so a case sets it there and fires a keystroke's event.
const setValue = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
const type = (input, value) =>
  act(() => {
    setValue.call(input, value);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
  });
/** The status line's own sentence, without the button that goes to the refused group. */
const statusOf = (container) => container.querySelector('dialog[open] .dialog__status')?.firstChild?.textContent ?? null;
/** The words a sighted reader sees: a page link's screen reader note that it opens a new tab is left out. */
const shownText = (el) => {
  if (!el) return null;
  const copy = el.cloneNode(true);
  for (const note of copy.querySelectorAll('.sr-only')) note.remove();
  return copy.textContent;
};
const fieldIn = (container, label) => [...container.querySelectorAll('dialog[open] .field')].find((f) => f.querySelector('.field__label')?.textContent?.trim() === label);

const mock = new MockApi();
const pools = await mock.listPools();
const accounts = await mock.listAccounts();

function poolDialog(props) {
  return h(PoolDialog, {
    accountsFailed: null,
    // The page opens each dialog's return rate on 7 days, so the dialog draws that range as it does there.
    idleReturns: { buckets: [], error: null, loading: false, onRange() {}, range: '7d' },
    keys: [],
    models: [],
    onClose() {},
    onKeyCreated() {},
    onKeysChanged() {},
    onRetryAccounts() {},
    onSaved() {},
    open: true,
    pools,
    sessions: [],
    ...props,
  });
}

test('the pool retention is unbounded with the short hint and the floor alone until the settings read lands the limit', async () => {
  const answer = holdSettings();
  const pool = pools.find((p) => p.bodyCaptureHours > 0);
  assert.ok(pool, 'the mock holds a pool that captures bodies');
  const { container, draw } = mount();
  await draw(poolDialog({ accounts, accountsLoaded: true, pool }));
  const field = () => container.querySelector('dialog[open] input[aria-label="Capture retention in hours"]');
  const hint = () => shownText(fieldIn(container, 'Retention (hours)')?.querySelector('.field__hint'));
  assert.equal(field()?.value, String(pool.bodyCaptureHours), 'the field opens on the stored retention');
  assert.equal(field()?.hasAttribute('max'), false, 'no ceiling stands before the read answers');
  assert.equal(hint(), "At most the bootstrap's body capture limit, shown on the Settings page's About card.");
  type(field(), '');
  assert.equal(statusOf(container), 'Save is held: Retention is a whole number of hours of at least 1.', 'an emptied field names the floor alone');
  answer();
  await settled();
  assert.equal(field()?.getAttribute('max'), String(LIMIT), 'the limit lands as the ceiling');
  assert.equal(hint(), `At most the bootstrap's body capture limit, ${LIMIT} hours here, shown on the Settings page's About card.`);
  assert.equal(statusOf(container), `Save is held: Retention is a whole number of hours from 1 to ${LIMIT}.`);
});

test("a stored member's row waits disabled for the account list, then the list's answer frees it with its badge", async () => {
  holdSettings();
  const pool = pools.find((p) => p.members.length > 1);
  assert.ok(pool, 'the mock holds a pool with more than one member');
  const missing = pool.members[0].accountId;
  const { container, draw } = mount();
  const rowOf = (id) => [...container.querySelectorAll('dialog[open] .members-table tbody tr')].find((tr) => tr.querySelector('td:first-child span')?.textContent === id);
  const read = (id) => {
    const row = rowOf(id);
    const box = row?.querySelector('input[type="checkbox"]');
    return { badge: row?.querySelector('.badge')?.textContent?.trim() ?? null, checked: box?.checked ?? null, disabled: box?.disabled ?? null, title: box?.getAttribute('title') ?? null };
  };
  // Before the read answers the page hands an empty list, so every stored member draws as a row of its own.
  await draw(poolDialog({ accounts: [], accountsLoaded: false, pool }));
  for (const m of pool.members) assert.deepEqual(read(m.accountId), { badge: null, checked: true, disabled: true, title: 'Waiting for the account list' }, `${m.accountId} waits for the list`);
  await draw(poolDialog({ accounts: accounts.filter((a) => a.id !== missing), accountsLoaded: true, pool }));
  assert.deepEqual(read(missing), { badge: 'not listed', checked: true, disabled: false, title: null }, 'the member the list lacks keeps its ticked row, enabled and marked');
});

test("the key override is unbounded with the floor's sentence until the settings read lands the limit", async () => {
  const answer = holdSettings();
  const key = (await mock.listKeys()).find((k) => k.id === 'desktop');
  assert.ok(key, 'the mock holds the desktop key');
  const keyPools = pools.map((p) => ({ autoModeServer: p.autoModeServer, bodyCaptureHours: p.bodyCaptureHours, id: p.id, members: p.members.map((m) => ({ accountId: m.accountId })), modelRules: p.modelRules, name: p.name }));
  const { container, draw } = mount();
  await draw(h(KeyDialog, { existing: key, existingIds: new Set(), onClose() {}, onCreated() {}, open: true, pools: keyPools }));
  const capture = () => [...container.querySelectorAll('dialog[open] section.dialog-group')].find((s) => s.getAttribute('aria-label') === 'Capture');
  const field = () => capture()?.querySelector('input[aria-label="Capture hours for this key"]');
  act(() => capture()?.querySelectorAll('.radio-list__item')[1]?.querySelector('input[type="radio"]')?.click());
  assert.equal(field()?.hasAttribute('max'), false, 'no ceiling stands before the read answers');
  type(field(), '');
  assert.equal(statusOf(container), 'Save is held: Capture hours is a whole number of at least 1.', 'an emptied field names the floor alone');
  answer();
  await settled();
  assert.equal(field()?.getAttribute('max'), String(LIMIT), 'the limit lands as the ceiling');
  assert.equal(statusOf(container), `Save is held: Capture hours is a whole number from 1 to ${LIMIT}.`);
});

test('an idle window set above the break-even reads a note under its field and its cost from the set value, while 0 and the other two ceilings read the cap', async () => {
  holdSettings();
  const pool = pools.find((p) => p.id === 'jobs');
  assert.ok(pool && pool.keepalive.maxIdleSeconds > pool.keepaliveResolved.breakEvenSeconds.max, 'the mock holds a pool whose idle window is set above its whole break-even');
  const { container, draw } = mount();
  await draw(poolDialog({ accounts, accountsLoaded: true, pool }));
  const BREAK_EVEN_HINT = "The pause length at which the recipe's probes cost one re-write of its prefix, for this pool's funding, cache TTL and models.";
  const keepalive = () => container.querySelector('dialog[open] section.dialog-group[aria-label="Keepalive"]');
  const linesOf = (el) => [...(el?.querySelectorAll('.keepalive-resolved') ?? [])].map((l) => l.textContent.replace(/\s+/g, ' ').trim());
  const idleField = () => fieldIn(container, 'Max idle (s)');
  const note = () => [...(idleField()?.querySelectorAll('.keepalive-resolved') ?? [])].find((l) => l.textContent.startsWith('Above the break-even'));
  assert.deepEqual(linesOf(idleField()), ['Costs 133% to 2880% of one re-write', 'Above the break-even, 46m to 17h 6m'], 'the cost line priced from the set day, then the note with the break-even it lies above');
  assert.equal(note()?.getAttribute('title'), BREAK_EVEN_HINT, "the note carries the break-even's hint");
  assert.equal(idleField()?.textContent.includes('capped at the break-even'), false, 'a window above the break-even reads no cap');
  // The pool's affinity of 0 blocks warming and with it every keepalive control, so the case raises the pin to an
  // hour and switches warming on before it reaches the two switched ceilings.
  const affinity = [...container.querySelectorAll('dialog[open] [aria-label="Session affinity"] .pill')].find((b) => b.textContent.trim() === '1h');
  act(() => affinity?.click());
  act(() => keepalive()?.querySelector('button[role="switch"][aria-label="Keep prompt caches warm"]')?.click());
  for (const label of ['Max parked (s)', 'Scheduled horizon (s)']) {
    act(() => keepalive()?.querySelector(`button[role="switch"][aria-label="${label}"]`)?.click());
    type(keepalive()?.querySelector(`input[aria-label="${label}"]`), '200000');
  }
  assert.deepEqual(
    linesOf(keepalive()),
    ['Costs 133% to 2880% of one re-write', 'Above the break-even, 46m to 17h 6m', 'Capped at the break-even, 46m to 17h 6m', 'Capped at the break-even, 46m to 17h 6m'],
    'the parked and horizon ceilings switched on past the break-even keep the cap the router applies to them',
  );
  type(idleField()?.querySelector('input'), '0');
  assert.deepEqual(linesOf(idleField()), ['Costs 133% to 2880% of one re-write at the saved 1d · capped at the break-even, 46m to 17h 6m'], 'a field at 0 reads the break-even it runs to beside the saved window, and no note');
  assert.equal(note(), undefined, 'no note while the field reads 0');
});

test('a new pool opens with the switch to keep prompt caches warm off, and free to turn on', async () => {
  holdSettings();
  const { container, draw } = mount();
  await draw(poolDialog({ accounts, accountsLoaded: true, pool: null }));
  const warm = container.querySelector('dialog[open] section.dialog-group[aria-label="Keepalive"] button[role="switch"][aria-label="Keep prompt caches warm"]');
  assert.ok(warm, 'the Keepalive group holds the switch');
  assert.equal(warm.getAttribute('aria-checked'), 'false', 'the switch starts off');
  assert.equal(warm.disabled, false, "the new form's affinity of an hour leaves the switch enabled, so off is the form's own value");
});
