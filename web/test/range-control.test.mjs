import assert from 'node:assert/strict';
import { after, afterEach, test } from 'node:test';
import { JSDOM } from 'jsdom';

// The span's words and the fields' minutes read the local zone, so the file reads them in UTC whatever the host's
// zone, and `node --test` gives each file a process of its own.
process.env.TZ = 'UTC';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true });
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'getComputedStyle', 'MouseEvent', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
after(() => window.close());
const { act, createElement: h, createRoot, RangeControl, spanText } = await import('./dist/ui.js');
const { fmtAbsolute, setTimeFormat, toLocalInput } = await import('./dist/api.js');

const at = (text) => Date.parse(text);
const iso = (ms) => new Date(ms).toISOString();
const NOW = at('2026-09-16T16:30:00.000Z');
const DAY_WINDOW = { from: NOW - 86_400_000, to: NOW };
const PRESET = { from: '', preset: '24h', to: '' };

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) act(() => root.unmount());
  document.body.replaceChildren();
});

/** The control under `props`, its writes collected, the clock standing at `now` unless a case moves it. */
const render = ({ now = () => NOW, value = PRESET, window: shown = DAY_WINDOW } = {}) => {
  const container = document.body.appendChild(document.createElement('div'));
  const root = createRoot(container);
  roots.push(root);
  const writes = [];
  act(() => root.render(h(RangeControl, { now, onChange: (next) => writes.push(next), value, window: shown })));
  return { container, writes };
};
const pills = (container) => [...container.querySelectorAll('.segmented[role="group"][aria-label="Range"] button')];
const custom = (container) => pills(container).at(-1);
const popover = () => document.querySelector('.popover');
const field = (label) => popover()?.querySelector(`input[aria-label="${label}"]`) ?? null;
const button = (text) => [...(popover()?.querySelectorAll('.popover__foot button') ?? [])].find((b) => b.textContent?.trim() === text) ?? null;
const line = () => [...(popover()?.querySelectorAll('span.secondary') ?? [])].map((s) => s.textContent?.trim()).join();
const click = (el) => act(() => el.click());
// React tracks an input's value through the prototype's setter, so a case sets it there and fires the input event a
// keystroke fires.
const setValue = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, 'value').set;
const type = (input, text) =>
  act(() => {
    setValue.call(input, text);
    input.dispatchEvent(new window.Event('input', { bubbles: true }));
  });

test('the span reads the times alone inside one day and the date before each time across days, on either clock', () => {
  const within = [at('2026-09-16T13:00:00Z'), at('2026-09-16T14:30:00Z')];
  const across = [at('2026-09-15T18:00:00Z'), at('2026-09-16T06:00:00Z')];
  assert.equal(spanText(...within), '13:00 to 14:30');
  assert.equal(spanText(...across), 'Sep 15 18:00 to Sep 16 06:00');
  setTimeFormat('12h');
  try {
    assert.match(spanText(...within), /^01:00\sPM to 02:30\sPM$/);
    assert.match(spanText(...across), /^Sep 15 06:00\sPM to Sep 16 06:00\sAM$/);
  } finally {
    setTimeFormat('24h');
  }
});

test('six pills print the short names they are named by, each preset titled with its label, the preset the address holds pressed with its start in its title, and the group title names its label and start', () => {
  const { container } = render();
  const group = container.querySelector('.segmented[role="group"]');
  assert.equal(group?.getAttribute('aria-label'), 'Range');
  assert.equal(group?.getAttribute('title'), `Last 24 hours, from ${fmtAbsolute(iso(DAY_WINDOW.from))}`);
  assert.deepEqual(
    pills(container).map((b) => b.textContent),
    ['1h', '6h', '24h', '7d', '30d', 'Custom'],
  );
  assert.deepEqual(
    pills(container).map((b) => b.getAttribute('aria-label')),
    [null, null, null, null, null, null],
    'a pill is named by the word it prints',
  );
  assert.deepEqual(
    pills(container).map((b) => b.getAttribute('title')),
    ['Last hour', 'Last 6 hours', `Last 24 hours, from ${fmtAbsolute(iso(DAY_WINDOW.from))}`, 'Last 7 days', 'Last 30 days', null],
    "the pressed pill covers the group's title, so it carries the start too",
  );
  assert.deepEqual(
    pills(container).map((b) => b.getAttribute('aria-pressed')),
    ['false', 'false', 'true', 'false', 'false', 'false'],
  );
  assert.equal(custom(container).getAttribute('title'), null, 'the Custom pill at rest shows the group title');
});

test('a preset click writes the preset with the instants dropped, and a click on the pressed preset writes nothing', () => {
  const { container, writes } = render({ value: { from: iso(NOW - 3_600_000), preset: 'custom', to: iso(NOW) }, window: { from: NOW - 3_600_000, to: NOW } });
  click(pills(container)[1]);
  assert.deepEqual(writes, [{ from: '', preset: '6h', to: '' }]);
  const pressed = render();
  click(pills(pressed.container)[2]);
  assert.deepEqual(pressed.writes, []);
});

test('a preset click with the Custom popover open closes it and writes the preset', () => {
  const { container, writes } = render();
  click(custom(container));
  assert.ok(popover() !== null, 'Custom opens the popover');
  click(pills(container)[1]);
  assert.equal(popover(), null, 'the preset click closes the popover');
  assert.deepEqual(writes, [{ from: '', preset: '6h', to: '' }]);
});

test('a second click on the Custom pill closes the popover and writes nothing', () => {
  const { container, writes } = render();
  click(custom(container));
  assert.ok(popover() !== null, 'the first click opens the popover');
  click(custom(container));
  assert.equal(popover(), null, 'the second click closes it');
  assert.equal(custom(container).getAttribute('aria-expanded'), 'false');
  assert.deepEqual(writes, []);
});

test('the Custom pill reads Custom at rest, and under a span reads pressed with the span and the pair in its title', () => {
  const from = at('2026-09-16T13:00:00Z');
  const to = at('2026-09-16T14:30:00Z');
  const { container } = render({ value: { from: iso(from), preset: 'custom', to: iso(to) }, window: { from, to } });
  const pill = custom(container);
  assert.equal(pill.textContent, '13:00 to 14:30');
  assert.equal(pill.getAttribute('aria-pressed'), 'true');
  assert.equal(pill.getAttribute('title'), `${fmtAbsolute(iso(from))} to ${fmtAbsolute(iso(to))}`);
  assert.equal(container.querySelector('.segmented')?.getAttribute('title'), `${fmtAbsolute(iso(from))} to ${fmtAbsolute(iso(to))}`);
  assert.equal(pills(container).filter((b) => b.getAttribute('aria-pressed') === 'true').length, 1);
  const rest = render();
  assert.equal(custom(rest.container).textContent, 'Custom');
});

test('the Custom pill opens a dialog named Custom range on the window the view shows, the To capped at the clock', () => {
  const { container, writes } = render({ now: () => NOW + 600_000 });
  assert.equal(popover(), null);
  click(custom(container));
  const box = popover();
  assert.equal(box?.getAttribute('role'), 'dialog');
  assert.equal(document.getElementById(box?.getAttribute('aria-labelledby') ?? '')?.textContent, 'Custom range');
  assert.equal(custom(container).getAttribute('aria-expanded'), 'true');
  assert.deepEqual([field('From')?.value, field('To')?.value], [toLocalInput(DAY_WINDOW.from), toLocalInput(DAY_WINDOW.to)]);
  assert.equal(field('To')?.getAttribute('max'), toLocalInput(NOW + 600_000));
  assert.equal(line(), '');
  assert.equal(button('Apply')?.disabled, false);
  assert.deepEqual(writes, [], 'opening writes nothing');
});

test("Apply writes both instants with the end capped at the clock read at Apply, and an unchanged field keeps the window's milliseconds", () => {
  let clock = NOW;
  const { container, writes } = render({ now: () => clock });
  click(custom(container));
  type(field('To'), toLocalInput(NOW + 2 * 3_600_000));
  clock = NOW + 45 * 60_000;
  click(button('Apply'));
  assert.deepEqual(writes, [{ from: iso(DAY_WINDOW.from), preset: 'custom', to: iso(NOW + 45 * 60_000) }]);
  assert.equal(popover(), null, 'Apply closes the popover');
});

test("an unchanged Apply writes the window's own instants to the millisecond, and a single edit keeps the other bound whole", () => {
  const shown = { from: at('2026-09-16T14:00:00.123Z'), to: at('2026-09-16T15:59:59.999Z') };
  const value = { from: iso(shown.from), preset: 'custom', to: iso(shown.to) };
  const { container, writes } = render({ value, window: shown });
  click(custom(container));
  assert.deepEqual([field('From')?.value, field('To')?.value], ['2026-09-16T14:00', '2026-09-16T15:59']);
  click(button('Apply'));
  click(custom(container));
  type(field('From'), '2026-09-16T13:00');
  click(button('Apply'));
  assert.deepEqual(writes, [
    { from: '2026-09-16T14:00:00.123Z', preset: 'custom', to: '2026-09-16T15:59:59.999Z' },
    { from: '2026-09-16T13:00:00.000Z', preset: 'custom', to: '2026-09-16T15:59:59.999Z' },
  ]);
});

test('a span inside one minute reads two equal fields and stays valid, written whole', () => {
  const shown = { from: at('2026-09-16T15:59:59.100Z'), to: at('2026-09-16T15:59:59.900Z') };
  const { container, writes } = render({ value: { from: iso(shown.from), preset: 'custom', to: iso(shown.to) }, window: shown });
  click(custom(container));
  assert.deepEqual([field('From')?.value, field('To')?.value], ['2026-09-16T15:59', '2026-09-16T15:59']);
  assert.equal(line(), '');
  assert.equal(button('Apply')?.disabled, false);
  click(button('Apply'));
  assert.deepEqual(writes, [{ from: '2026-09-16T15:59:59.100Z', preset: 'custom', to: '2026-09-16T15:59:59.900Z' }]);
});

test('a reversed pair, an unreadable field and two future dates each hold Apply off with the line drawn', () => {
  const { container, writes } = render();
  click(custom(container));
  type(field('From'), toLocalInput(NOW + 60_000));
  assert.equal(line(), 'End must be after start.', 'a From after the To');
  assert.equal(button('Apply')?.disabled, true);
  type(field('From'), '');
  assert.equal(line(), 'End must be after start.', 'an empty From');
  assert.equal(button('Apply')?.disabled, true);
  // The end is capped before the pair is judged, so a pair wholly after the clock ends before it starts.
  type(field('From'), toLocalInput(NOW + 3_600_000));
  type(field('To'), toLocalInput(NOW + 2 * 3_600_000));
  assert.equal(line(), 'End must be after start.', 'two future dates');
  assert.equal(button('Apply')?.disabled, true);
  click(button('Apply'));
  assert.deepEqual(writes, []);
  type(field('From'), toLocalInput(NOW - 3_600_000));
  assert.equal(line(), '');
  assert.equal(button('Apply')?.disabled, false);
});

test('Cancel and Escape close the popover and write nothing, and the next opening starts from the window again', () => {
  const { container, writes } = render();
  click(custom(container));
  type(field('From'), '2026-09-16T10:00');
  click(button('Cancel'));
  assert.equal(popover(), null);
  click(custom(container));
  assert.equal(field('From')?.value, toLocalInput(DAY_WINDOW.from));
  act(() => field('From').dispatchEvent(new window.KeyboardEvent('keydown', { bubbles: true, cancelable: true, key: 'Escape' })));
  assert.equal(popover(), null);
  // A boolean, since a failed read of two jsdom elements hands the reporter the whole window to serialize.
  assert.ok(document.activeElement === custom(container), 'Escape returns the focus to the pill');
  assert.deepEqual(writes, []);
});
