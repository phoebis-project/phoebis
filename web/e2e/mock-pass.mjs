import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { fanOut, selectParts } from './parts.mjs';
import { serve } from './serve.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const bundle = join(here, '..', 'dist-mock');
if (!existsSync(join(bundle, 'index.html'))) {
  console.error(`no mock bundle under ${bundle}; run npm run build:mock first`);
  process.exit(2);
}

/**
 * The mock's story is anchored at load and bucketed by the hour, so the pass runs on a fixed weekday afternoon
 * in UTC: the same instant the smoke run pins, which is what makes two passes over one tree match.
 */
const CLOCK = '2026-09-16T14:30:00Z';

/**
 * The keys of `WALKS` in `views.mjs` under the mock, copied since that file runs its pass on import: a walk added there
 * runs here only once this list names it. The mock's story lives in each page, so no part reads another's writes, and
 * each part runs in a `views.mjs` of its own beside the others.
 */
const WALK_KEYS = ['light', 'dark', 'light-github', 'dark-github'];
let parts;
try {
  parts = selectParts(process.env.PHOEBIS_E2E_WALKS, WALK_KEYS);
} catch (error) {
  console.error(error.message);
  process.exit(2);
}

const server = await serve(bundle);
const env = { ...process.env, PHOEBIS_E2E_BASE: `http://127.0.0.1:${server.port}`, PHOEBIS_E2E_CLOCK: CLOCK, PHOEBIS_E2E_MOCK: '1', PHOEBIS_E2E_SECRET: 'admin' };
const pass = await fanOut(parts, (part) => spawn(process.execPath, [join(here, 'views.mjs')], { env: { ...env, PHOEBIS_E2E_WALKS: part }, stdio: ['inherit', 'pipe', 'pipe'] }));
server.close();
// Each child's output whole, in the parts' order, so the lines of two children never mix.
for (const run of pass.runs) for (const { chunk, stream } of run.output) process[stream].write(chunk);
process.exit(pass.status);
