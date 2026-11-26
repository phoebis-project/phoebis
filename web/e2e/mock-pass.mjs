import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { fanOut, selectParts, walksOf } from './parts.mjs';
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
 * The mock's story lives in each page, so no part reads another's writes, and each part runs in a `views.mjs` of its
 * own beside the others.
 */
let parts;
try {
  parts = selectParts(process.env.PHOEBIS_E2E_WALKS, walksOf(true).map((walk) => walk.key));
} catch (error) {
  console.error(error.message);
  process.exit(2);
}

const server = await serve(bundle);
const env = { ...process.env, PHOEBIS_E2E_BASE: `http://127.0.0.1:${server.port}`, PHOEBIS_E2E_CLOCK: CLOCK, PHOEBIS_E2E_MOCK: '1', PHOEBIS_E2E_SECRET: 'admin' };
const children = [];
// A signal sent to the pass alone, as a job runner's cancel sends it, would leave the walks and their browsers running
// until their own end.
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => children.forEach((child) => child.kill(signal)));
const pass = await fanOut(parts, (part) => {
  const child = spawn(process.execPath, [join(here, 'views.mjs')], { env: { ...env, PHOEBIS_E2E_WALKS: part }, stdio: ['inherit', 'pipe', 'pipe'] });
  children.push(child);
  return child;
});
server.close();
// Each child's output whole, in the parts' order, so the lines of two children never mix.
for (const run of pass.runs) for (const { chunk, stream } of run.output) process[stream].write(chunk);
// The status is set and the process ends when its handles close: an exit call drops the output above that a pipe
// has not taken yet.
process.exitCode = pass.status;
