import react from '@vitejs/plugin-react';
import { fileURLToPath, URL } from 'node:url';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) },
  },
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
