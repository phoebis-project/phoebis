import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

const sheet = (name) => readFileSync(new URL(`../src/styles/${name}.css`, import.meta.url), 'utf8');

/** The rules whose `--ink-3` draws no word the operator reads: placeholders, disabled controls and decorative strokes. */
const INK3_ALLOWED = new Set([
  '.chip--removable button',
  '.dialog__close',
  '.dialog-group .input:disabled',
  '.icon-btn',
  '.input::placeholder',
  '.ldot',
  '.mselect__clear',
  '.pct-label',
  '.quota-meta__sep',
  '.search > svg',
  '.select > svg',
  '.srow__chevron',
  '.status-dot',
]);

/**
 * Every style rule of a sheet with its own declarations and the at-rules it sits in. A rule nested in `@media` is
 * still a rule, so a text rule cannot hide from the allowlist behind a breakpoint.
 */
function rules(css) {
  const text = css.replace(/\/\*[\s\S]*?\*\//g, '');
  const out = [];
  const open = [];
  let buf = '';
  for (const c of text) {
    if (c === '{') {
      open.push({ decls: [], prelude: buf.replace(/\s+/g, ' ').trim() });
      buf = '';
    } else if (c === ';' || c === '}') {
      if (buf.trim() && open.length > 0) open.at(-1).decls.push(buf.trim());
      buf = '';
      if (c === '}') {
        const block = open.pop();
        if (block && !block.prelude.startsWith('@')) {
          out.push({ decls: block.decls, selectors: block.prelude.split(',').map((s) => s.trim()), within: open.map((b) => b.prelude) });
        }
      }
    } else {
      buf += c;
    }
  }
  assert.equal(open.length, 0, 'every block the sheet opens closes');
  return out;
}

function tokens(selector) {
  const block = rules(sheet('tokens')).find((r) => r.selectors.length === 1 && r.selectors[0] === selector);
  assert.ok(block, `tokens.css has a ${selector} block`);
  return Object.fromEntries(block.decls.filter((d) => d.startsWith('--')).map((d) => d.split(':').map((s) => s.trim())));
}

function luminance(hex) {
  const m = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(hex);
  assert.ok(m, `${hex} is a six-digit hex colour`);
  const [r, g, b] = m.slice(1).map((h) => {
    const s = parseInt(h, 16) / 255;
    return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

const contrast = (a, b) => {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
};

test('the text inks reach 4.5 to 1 on every surface in both themes', () => {
  const light = tokens(':root');
  const themes = { dark: { ...light, ...tokens(":root[data-theme='dark']") }, light };
  const short = [];
  for (const [theme, t] of Object.entries(themes)) {
    for (const ink of ['--ink', '--ink-2']) {
      for (const surface of ['--page', '--surface', '--surface-2', '--surface-3']) {
        const ratio = contrast(t[ink], t[surface]);
        if (ratio < 4.5) short.push(`${theme} ${ink} on ${surface}: ${ratio.toFixed(2)}`);
      }
    }
  }
  assert.deepEqual(short, []);
});

test('only placeholders, disabled controls and decorative strokes read --ink-3', () => {
  const stray = [];
  for (const name of ['base', 'components', 'pages']) {
    for (const rule of rules(sheet(name))) {
      if (!rule.decls.some((d) => /var\(\s*--ink-3\b/.test(d))) continue;
      for (const selector of rule.selectors) {
        if (!INK3_ALLOWED.has(selector)) stray.push(`${name}.css ${[...rule.within, selector].join(' ')}`);
      }
    }
  }
  assert.deepEqual(stray, []);
});
