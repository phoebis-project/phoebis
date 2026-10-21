import react from '@vitejs/plugin-react';
import { execFileSync } from 'node:child_process';
import { fileURLToPath, URL } from 'node:url';
import { defineConfig } from 'vite';

/**
 * The commit the bundle is built from, read from the router's sources in the router's order: `PHOEBIS_BUILD_COMMIT`
 * (blank reads as unset, as the router reads it), then `git rev-parse HEAD` in this tree, then `unknown`. Trimmed,
 * since git's answer ends in a newline the router's header never carries, and lowercased like the header's hash.
 */
function buildCommit(): string {
  const named = process.env.PHOEBIS_BUILD_COMMIT?.trim();
  if (named) return named.toLowerCase();
  try {
    const head = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: fileURLToPath(new URL('.', import.meta.url)), encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim();
    return head === '' ? 'unknown' : head.toLowerCase();
  } catch {
    return 'unknown';
  }
}

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) },
  },
  define: { __PHOEBIS_BUILD__: JSON.stringify(buildCommit()) },
  build: {
    target: 'es2022',
    // The console and the API reference page are two entries of one bundle, so the ui crate embeds one dist/
    // and the page's hashed assets sit beside the console's. An entry's chunk is named after its key, and the
    // console's stays `index` for the `index-<hash>.js` name the release's binary smoke looks for.
    rolldownOptions: {
      input: {
        index: fileURLToPath(new URL('./index.html', import.meta.url)),
        'api-reference': fileURLToPath(new URL('./api-reference/index.html', import.meta.url)),
      },
    },
  },
  server: {
    proxy: {
      '/admin': { target: 'http://127.0.0.1:8081' },
      '/api-reference/openapi.json': { target: 'http://127.0.0.1:8081' },
    },
  },
});
