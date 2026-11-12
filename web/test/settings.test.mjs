import assert from 'node:assert/strict';
import { after, test } from 'node:test';
import { JSDOM } from 'jsdom';

// React reads whether a DOM exists when its module loads, so the window stands before the bundle is imported. The
// window has an origin, so its storage answers, and the getter hands the bundle that storage in place of Node's own,
// whose one file every suite of the run shares.
const { window } = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true, url: 'http://localhost/' });
for (const key of ['document', 'navigator', 'HTMLElement', 'Element', 'Node', 'Event']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.window = window;
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
// The settings provider follows the color scheme through matchMedia, which jsdom lacks.
window.matchMedia = () => ({ matches: false, addEventListener() {}, removeEventListener() {} });
Object.defineProperty(globalThis, 'localStorage', { configurable: true, get: () => window.localStorage });
after(() => window.close());
const { act, AppSettingsProvider, createElement: h, createRoot, getTimeFormat, setTimeFormat } = await import('./dist/ui.js');

test('a stored time format outside the union reads 24h', (t) => {
  // The module starts in the other format, so only a provider that applies the default passes.
  const before = getTimeFormat();
  setTimeFormat('12h');
  window.localStorage.setItem('phoebis.timeFormat', '13h');
  const root = createRoot(document.body.appendChild(document.createElement('div')));
  t.after(() => {
    act(() => root.unmount());
    window.localStorage.clear();
    setTimeFormat(before);
  });
  act(() => root.render(h(AppSettingsProvider, null)));
  assert.equal(getTimeFormat(), '24h');
});

/** The palette the provider puts on the root, with `stored` under its key before the render. */
function appliedPalette(t, stored) {
  window.localStorage.setItem('phoebis.palette', stored);
  const root = createRoot(document.body.appendChild(document.createElement('div')));
  t.after(() => {
    act(() => root.unmount());
    window.localStorage.removeItem('phoebis.palette');
    document.documentElement.removeAttribute('data-palette');
  });
  act(() => root.render(h(AppSettingsProvider, null)));
  return document.documentElement.getAttribute('data-palette');
}

test('a stored github palette draws the GitHub palette', (t) => {
  assert.equal(appliedPalette(t, 'github'), 'github');
});

test('a stored palette outside the union draws the default palette', (t) => {
  assert.equal(appliedPalette(t, 'solarized'), 'default');
});
