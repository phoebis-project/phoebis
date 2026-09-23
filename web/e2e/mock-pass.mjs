import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
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

const server = await serve(bundle);
const child = spawn(process.execPath, [join(here, 'views.mjs')], {
  env: { ...process.env, PHOEBIS_E2E_BASE: `http://127.0.0.1:${server.port}`, PHOEBIS_E2E_CLOCK: CLOCK, PHOEBIS_E2E_MOCK: '1', PHOEBIS_E2E_SECRET: 'admin' },
  stdio: 'inherit',
});
const code = await new Promise((resolve) => {
  child.on('error', (error) => {
    console.error(`the browser pass did not start: ${error.message}`);
    resolve(1);
  });
  child.on('exit', (status) => resolve(status ?? 1));
});
server.close();
process.exit(code);
