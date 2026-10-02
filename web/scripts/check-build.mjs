#!/usr/bin/env node
// Holds the production build to a split entry: the chart pages load as their own chunks, so the entry carries no
// Recharts code and stays under the bundler's chunk-size advisory. Run from web/ after `npm run build`; it reads
// the built files rather than a manifest, since the UI crate embeds all of dist/ and would serve one.
import { readdirSync, readFileSync } from 'node:fs';
import { basename, dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const dist = join(dirname(fileURLToPath(import.meta.url)), '..', 'dist');
// Kilobytes of 1000 bytes, the unit the bundler's advisory counts in.
const ENTRY_LIMIT_KB = 500;

const html = readFileSync(join(dist, 'index.html'), 'utf8');
// The classic theme script comes first in the head and is not a chunk; the entry is the module script.
const tag = html.match(/<script\b[^>]*\btype="module"[^>]*>/);
const src = tag?.[0].match(/\bsrc="([^"]+)"/)?.[1];
if (!src) {
  console.error('dist/index.html names no module script');
  process.exit(1);
}
const entry = basename(src);
const chunks = readdirSync(join(dist, 'assets')).filter((name) => name.endsWith('.js'));
const text = readFileSync(join(dist, 'assets', entry), 'utf8');
const kb = Buffer.byteLength(text) / 1000;
console.log(`chunks ${chunks.length}, entry ${entry} ${kb.toFixed(1)} kB`);

const failures = [];
if (chunks.length === 1) failures.push('the build emitted one chunk');
if (kb > ENTRY_LIMIT_KB) failures.push(`the entry passes ${ENTRY_LIMIT_KB} kB`);
if (text.includes('recharts-')) failures.push('the entry carries Recharts code');
for (const failure of failures) console.error(failure);
process.exit(failures.length ? 1 : 0);
