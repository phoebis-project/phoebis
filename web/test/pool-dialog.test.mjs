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
const { act, api, ApiError, AppSettingsProvider, createElement: h, createRoot, fakeClock, installClock, KeyDialog, MemoryRouter, PoolDialog } = await import('./dist/ui.js');
const { MockApi } = await import('./dist/api.js');

// The bootstrap's limit the page's settings read hands the dialogs, the test's own figure.
const LIMIT = 72;

const roots = [];
const reads = { getKeepaliveOverview: api.getKeepaliveOverview, getKeyModels: api.getKeyModels, listCatalogOf: api.listCatalogOf, listSessions: api.listSessions, updateKey: api.updateKey, updatePool: api.updatePool };
beforeEach(() => {
  // A frozen clock, so the dialog's ticking instant arms no window timer.
  const clock = fakeClock();
  clock.freeze(Date.UTC(2026, 9, 14, 12));
  installClock(clock);
  // The test build's api is the HTTP client, so the dialogs' other reads answer here: the members' catalogs after the
  // suggestions' pause and an open edit's model list empty, a saved pool's sessions and return rate as the mock does.
  api.listCatalogOf = async () => [];
  api.getKeyModels = async (keyId) => ({ keyId, longContextTwins: true, modelRulesMode: 'extend', poolId: '', rules: [] });
  api.getKeepaliveOverview = async ({ range }) => overviews[range];
  api.listSessions = async () => sessions;
});
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
  Object.assign(api, reads);
  installClock(null);
});

// A read answers on a promise, which settles after a synchronous act has returned, so the wait steps out of the
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

// A saved pool's dialog reads the sessions and its return rate itself, answered here as the mock answers them.
const sessions = await mock.listSessions();
const overviews = { '30d': await mock.getKeepaliveOverview({ range: '30d' }), '7d': await mock.getKeepaliveOverview({ range: '7d' }) };
/** The accounts read as the page hands it down, its list null until it answers. */
const accountsRead = (data) => ({ data, error: null, reload() {} });

function poolDialog(props) {
  return h(PoolDialog, {
    keys: [],
    models: [],
    onClose() {},
    onKeysChanged() {},
    onSaved() {},
    open: true,
    pools,
    ...props,
  });
}

test('the pool retention is unbounded with the short hint and the floor alone until the page hands the limit down as a prop', async () => {
  const pool = pools.find((p) => p.bodyCaptureHours > 0);
  assert.ok(pool, 'the mock holds a pool that captures bodies');
  const { container, draw } = mount();
  await draw(poolDialog({ accounts: accountsRead(accounts), pool }));
  const field = () => container.querySelector('dialog[open] input[aria-label="Capture retention in hours"]');
  const hint = () => shownText(fieldIn(container, 'Retention (hours)')?.querySelector('.field__hint'));
  assert.equal(field()?.value, String(pool.bodyCaptureHours), 'the field opens on the stored retention');
  assert.equal(field()?.hasAttribute('max'), false, 'no ceiling stands until the page hands the limit down');
  assert.equal(hint(), "At most the bootstrap's body capture limit, shown on the Settings page's About card.");
  type(field(), '');
  assert.equal(statusOf(container), 'Save is held: Retention is a whole number of hours of at least 1.', 'an emptied field names the floor alone');
  await draw(poolDialog({ accounts: accountsRead(accounts), captureMaxHours: LIMIT, pool }));
  assert.equal(field()?.getAttribute('max'), String(LIMIT), 'the limit lands as the ceiling');
  assert.equal(hint(), `At most the bootstrap's body capture limit, ${LIMIT} hours here, shown on the Settings page's About card.`);
  assert.equal(statusOf(container), `Save is held: Retention is a whole number of hours from 1 to ${LIMIT}.`);
});

test("a stored member's row waits disabled for the account list, then the list's answer frees it with its badge", async () => {
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
  // Before the read answers it holds no list, so every stored member draws as a row of its own.
  await draw(poolDialog({ accounts: accountsRead(null), pool }));
  for (const m of pool.members) assert.deepEqual(read(m.accountId), { badge: null, checked: true, disabled: true, title: 'Waiting for the account list' }, `${m.accountId} waits for the list`);
  await draw(poolDialog({ accounts: accountsRead(accounts.filter((a) => a.id !== missing)), pool }));
  assert.deepEqual(read(missing), { badge: 'not listed', checked: true, disabled: false, title: null }, 'the member the list lacks keeps its ticked row, enabled and marked');
});

test('a pool dialog whose account list failed draws the failure with its retry, and no member row waits', async () => {
  const pool = pools.find((p) => p.members.length > 0);
  assert.ok(pool, 'the mock holds a pool with members');
  let reloads = 0;
  const { container, draw } = mount();
  await draw(poolDialog({ accounts: { data: null, error: 'HTTP 503', reload: () => (reloads += 1) }, pool }));
  const members = container.querySelector('dialog[open] section.dialog-group[aria-label="Members"]');
  const failed = members?.querySelector('.empty--error');
  assert.deepEqual([failed?.querySelector('.empty__title')?.textContent, failed?.querySelector('.empty__body')?.textContent], ['The account list did not load', 'HTTP 503']);
  assert.equal(members?.querySelectorAll('.members-table input[type="checkbox"][disabled][title="Waiting for the account list"]').length, 0, 'no stored member waits for a list that failed');
  act(() => failed?.querySelector('button')?.click());
  assert.equal(reloads, 1, 'Try again reads the list again');
});

test("the key override is unbounded with the floor's sentence until the page hands the limit down as a prop", async () => {
  const key = (await mock.listKeys()).find((k) => k.id === 'desktop');
  assert.ok(key, 'the mock holds the desktop key');
  const keyPools = pools.map((p) => ({ autoModeServer: p.autoModeServer, bodyCaptureHours: p.bodyCaptureHours, id: p.id, members: p.members.map((m) => ({ accountId: m.accountId })), modelRules: p.modelRules, name: p.name }));
  const { container, draw } = mount();
  const keyDialog = (captureMaxHours) => h(KeyDialog, { captureMaxHours, existing: key, existingIds: new Set(), onClose() {}, onCreated() {}, open: true, pools: keyPools });
  await draw(keyDialog(undefined));
  const capture = () => [...container.querySelectorAll('dialog[open] section.dialog-group')].find((s) => s.getAttribute('aria-label') === 'Capture');
  const field = () => capture()?.querySelector('input[aria-label="Capture hours for this key"]');
  act(() => capture()?.querySelectorAll('.radio-list__item')[1]?.querySelector('input[type="radio"]')?.click());
  assert.equal(field()?.hasAttribute('max'), false, 'no ceiling stands until the page hands the limit down');
  type(field(), '');
  assert.equal(statusOf(container), 'Save is held: Capture hours is a whole number of at least 1.', 'an emptied field names the floor alone');
  await draw(keyDialog(LIMIT));
  assert.equal(field()?.getAttribute('max'), String(LIMIT), 'the limit lands as the ceiling');
  assert.equal(statusOf(container), `Save is held: Capture hours is a whole number from 1 to ${LIMIT}.`);
});

test("a key dialog keeps a failed save's words while a field is written again unchanged, and drops them on the next edit", async () => {
  const desktop = (await mock.listKeys()).find((k) => k.id === 'desktop');
  assert.ok(desktop, 'the mock holds the desktop key');
  // A stored override opens the dialog on it, so the override field's focus writes the mode the form already holds.
  const key = { ...desktop, bodyCaptureHours: 12 };
  api.updateKey = async () => {
    throw new ApiError(422, 'The pool refused the key', null);
  };
  const { container, draw } = mount();
  await draw(h(KeyDialog, { captureMaxHours: LIMIT, existing: key, existingIds: new Set(), onClose() {}, onCreated() {}, open: true, pools }));
  act(() => [...container.querySelectorAll('dialog[open] .dialog__actions button')].find((b) => b.textContent?.trim() === 'Save')?.click());
  await settled();
  assert.equal(statusOf(container), 'The pool refused the key');
  act(() => container.querySelector('dialog[open] input[aria-label="Capture hours for this key"]')?.focus());
  assert.equal(statusOf(container), 'The pool refused the key', "the override field's focus keeps the words");
  type(fieldIn(container, 'Name')?.querySelector('input'), 'Renamed key');
  assert.equal(statusOf(container), 'Every group saves together.', 'a changed name drops them');
});

test('an idle window set above the break-even reads a note under its field and its cost from the set value, while 0 and the other two ceilings read the cap', async () => {
  const pool = pools.find((p) => p.id === 'jobs');
  assert.ok(pool && pool.keepalive.maxIdleSeconds > pool.keepaliveResolved.breakEvenSeconds.max, 'the mock holds a pool whose idle window is set above its whole break-even');
  const { container, draw } = mount();
  await draw(poolDialog({ accounts: accountsRead(accounts), pool }));
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

test('an emptied Priority holds the save under its sentence', async () => {
  const pool = pools.find((p) => p.members.length > 0);
  assert.ok(pool, 'the mock holds a pool with members');
  const { container, draw } = mount();
  await draw(poolDialog({ accounts: accountsRead(accounts), pool }));
  type(container.querySelector('dialog[open] .members-table input[aria-label^="Priority for "]:not([disabled])'), '');
  assert.equal(statusOf(container), 'Save is held: Priority is a whole number of 0 or more.');
});

test('text the browser cannot parse in Max idle holds the save under its sentence, where a number it parses saves', async () => {
  const pool = pools.find((p) => p.keepalive.enabled && p.affinityTtlSeconds >= 300);
  assert.ok(pool, 'the mock holds a pool that keeps prompt caches warm, so its idle window takes an entry');
  const { container, draw } = mount();
  await draw(poolDialog({ accounts: accountsRead(accounts), pool }));
  const idle = fieldIn(container, 'Max idle (s)')?.querySelector('input');
  type(idle, '600');
  assert.equal(statusOf(container), 'Every group saves together.', 'a number the browser parses saves');
  // A browser empties a number field's value for text it cannot parse and reports it through the field's validity,
  // which jsdom never does.
  Object.defineProperty(idle, 'validity', { configurable: true, value: { badInput: true } });
  type(idle, '');
  assert.equal(statusOf(container), 'Save is held: Max idle (s) is a whole number from 0 to 31,536,000.');
});

test('text the browser cannot parse in a Daily budget holds the save, where an emptied budget saves as none', async () => {
  // The pool all's member primary opens with no daily budget (`web/src/mock/pools.ts`).
  const pool = pools.find((p) => p.id === 'all');
  assert.ok(pool, 'the mock holds the pool all');
  const { container, draw } = mount();
  await draw(poolDialog({ accounts: accountsRead(accounts), pool }));
  const daily = container.querySelector('dialog[open] input[aria-label="Daily budget for primary, USD"]');
  type(daily, '12');
  type(daily, '');
  assert.equal(statusOf(container), 'Every group saves together.', 'an emptied budget is none');
  Object.defineProperty(daily, 'validity', { configurable: true, value: { badInput: true } });
  type(daily, '');
  assert.equal(statusOf(container), 'Save is held: A budget is a dollar amount above 0. Leave it empty for no budget.');
});

test('an emptied Hold (s) or Stretch (s) leaves the slow-down preview on the figure the dialog opened with, while the field holds the save', async () => {
  // The pool work slows its member studio to half speed, studio's served stretch reads 42 min, and the pool stores a
  // hold of 120 s and a stretch of 1,800 s, where a new pool's defaults read 120 s and 9,000 s
  // (`web/src/mock/pools.ts`, `web/src/mock/accounts.ts`).
  const pool = pools.find((p) => p.id === 'work');
  assert.ok(pool?.slowDown, 'the mock holds the pool work with its slow-down on');
  const { container, draw } = mount();
  // Every field of the Slow-down group is expert, so opening on it turns the expert view on.
  await draw(poolDialog({ accounts: accountsRead(accounts), initialGroup: 'slowDown', pool }));
  const preview = () => container.querySelector('dialog[open] .slow-down-preview')?.textContent ?? '';
  const opened = preview();
  assert.ok(opened.endsWith('up to 120 s.'), `the preview names the stored hold: ${opened}`);
  type(fieldIn(container, 'Hold (s)')?.querySelector('input'), '');
  assert.equal(statusOf(container), 'Save is held: Hold (s) is a whole number from 1 to 300.');
  assert.equal(preview(), opened, 'the preview keeps the stored hold');
  // The stored stretch of 1,800 s reads 30 min, where the default's 9,000 s would read 2 h 30 min.
  type(fieldIn(container, 'Stretch (s)')?.querySelector('input'), '');
  assert.equal(preview(), '1 member is slowed: studio at 0.5x (42 min of 30 min). A session there waits its cycle times 1.0, up to 120 s.', 'the preview keeps the stored stretch');
});

test("a ticked member's emptied Priority stands last among the members and above every account that is not one", async () => {
  // The pool all holds nightly, primary and secondary at 0 and Overflow workspace at 1, and contract is the first
  // account by label outside it (`web/src/mock/pools.ts`).
  const pool = pools.find((p) => p.id === 'all');
  assert.ok(pool, 'the mock holds the pool all');
  const { container, draw } = mount();
  await draw(poolDialog({ accounts: accountsRead(accounts), pool }));
  const priority = container.querySelector('dialog[open] input[aria-label="Priority for primary"]');
  act(() => priority?.focus());
  type(priority, '');
  act(() => priority?.blur());
  const rows = [...container.querySelectorAll('dialog[open] .members-table tbody tr')].slice(0, 5).map((tr) => [tr.querySelector('.members-table__name')?.textContent, tr.querySelector('input[type="checkbox"]')?.checked ?? false]);
  assert.deepEqual(rows, [['nightly', true], ['secondary', true], ['Overflow workspace', true], ['primary', true], ['contract', false]]);
});

test('a new pool opens with the switch to keep prompt caches warm off, and free to turn on', async () => {
  const { container, draw } = mount();
  await draw(poolDialog({ accounts: accountsRead(accounts), pool: null }));
  const warm = container.querySelector('dialog[open] section.dialog-group[aria-label="Keepalive"] button[role="switch"][aria-label="Keep prompt caches warm"]');
  assert.ok(warm, 'the Keepalive group holds the switch');
  assert.equal(warm.getAttribute('aria-checked'), 'false', 'the switch starts off');
  assert.equal(warm.disabled, false, "the new form's affinity of an hour leaves the switch enabled, so off is the form's own value");
});

test('a save hands the client the Thinking diagnostics switch as the dialog shows it', async () => {
  // The pool jobs stores both diagnostics switches off (`web/src/mock/pools.ts`).
  const pool = pools.find((p) => p.id === 'jobs');
  assert.ok(pool && !pool.cacheDiagnostics && !pool.thinkingDiagnostics, 'the mock holds the pool jobs with both diagnostics switches off');
  const sent = [];
  api.updatePool = async (id, input) => {
    sent.push(input.thinkingDiagnostics);
    return pool;
  };
  const switchIn = (container, label) => container.querySelector(`dialog[open] button[role="switch"][aria-label="${label}"]`);
  const save = (container) => act(() => [...container.querySelectorAll('dialog[open] .dialog__actions button')].find((b) => b.textContent?.trim() === 'Save pool')?.click());
  // Each field of the group is expert, so opening on it turns the expert view on.
  const untouched = mount();
  await untouched.draw(poolDialog({ accounts: accountsRead(accounts), initialGroup: 'cacheDiagnostics', pool }));
  save(untouched.container);
  await settled();
  const ticked = mount();
  await ticked.draw(poolDialog({ accounts: accountsRead(accounts), initialGroup: 'cacheDiagnostics', pool }));
  // Cache diagnostics goes on first, so a Thinking switch that showed its neighbor's state would turn itself off.
  act(() => switchIn(ticked.container, 'Cache diagnostics')?.click());
  act(() => switchIn(ticked.container, 'Thinking diagnostics')?.click());
  save(ticked.container);
  await settled();
  assert.deepEqual(sent, [false, true], 'the untouched dialog sends the switch off and the ticked one sends it on');
});

test("a key move sends the stored key's settings with the new pool, the name trimmed and an empty note as none", async () => {
  const pool = pools[0];
  const target = pools.find((p) => p.id !== pool.id);
  assert.ok(target, 'the mock holds a second pool to move the key to');
  // Every setting off the key dialog's defaults, and a name and a note that the dialog's own save would never store.
  const stored = {
    autoModeServer: false,
    bodyCaptureHours: 12,
    companionPermissions: [],
    enabled: false,
    expiresAt: '2026-12-01T00:00:00.000Z',
    guardrails: [],
    id: 'mover',
    longContextTwins: false,
    modelRules: [{ not: true, pattern: 'claude-opus-*' }],
    modelRulesMode: 'replace',
    name: '  mover  ',
    notes: '',
    poolId: pool.id,
    prefix: 'phx_mover',
    revision: 7,
    status: 'active',
  };
  const sent = [];
  api.updateKey = async (id, input, revision) => {
    sent.push({ id, input, revision });
    return { ...stored, poolId: input.poolId };
  };
  const { container, draw } = mount();
  await draw(poolDialog({ accounts: accountsRead(accounts), keys: [stored], pool }));
  const row = [...container.querySelectorAll('dialog[open] .keys-table tbody tr')].find((tr) => tr.children[0]?.textContent === stored.name);
  act(() => row?.querySelector('.menu__trigger')?.click());
  act(() => [...(row?.querySelectorAll('[role="menuitem"]') ?? [])].find((b) => b.textContent?.trim() === 'Move to…')?.click());
  const move = [...document.querySelectorAll('dialog[open]')].pop();
  const select = move?.querySelector('select[aria-label="Destination pool"]');
  act(() => {
    Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set.call(select, target.id);
    select.dispatchEvent(new window.Event('change', { bubbles: true }));
  });
  act(() => [...(move?.querySelectorAll('.dialog__foot button') ?? [])].find((b) => b.textContent?.trim() === 'Move key')?.click());
  await settled();
  assert.equal(sent.length, 1, 'the move writes once');
  assert.equal(sent[0].id, 'mover');
  assert.equal(sent[0].revision, 7, "the write carries the stored key's revision");
  assert.deepStrictEqual(sent[0].input, {
    autoModeServer: false,
    bodyCaptureHours: 12,
    companionPermissions: [],
    enabled: false,
    expiresAt: undefined,
    longContextTwins: false,
    modelRules: [{ not: true, pattern: 'claude-opus-*' }],
    modelRulesMode: 'replace',
    name: 'mover',
    notes: null,
    poolId: target.id,
  });
});

test('a pool dialog reopened after a 30-day pick reads its return rate once, over 7 days', async () => {
  const pool = pools[0];
  const ranges = [];
  api.getKeepaliveOverview = async ({ range }) => {
    ranges.push(range);
    return overviews[range];
  };
  const { container, draw } = mount();
  await draw(poolDialog({ accounts: accountsRead(accounts), pool }));
  const select = container.querySelector('dialog[open] select[aria-label="Return rate range"]');
  act(() => {
    Object.getOwnPropertyDescriptor(window.HTMLSelectElement.prototype, 'value').set.call(select, '30d');
    select.dispatchEvent(new window.Event('change', { bubbles: true }));
  });
  await settled();
  assert.deepEqual(ranges, ['7d', '30d'], 'the opening reads 7 days and the pick 30 days');
  await draw(poolDialog({ accounts: accountsRead(accounts), open: false, pool: null }));
  ranges.length = 0;
  await draw(poolDialog({ accounts: accountsRead(accounts), pool }));
  assert.deepEqual(ranges, ['7d'], 'the reopening reads once, over 7 days');
});
