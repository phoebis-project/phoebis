import react from '@vitejs/plugin-react';
import { fileURLToPath, URL } from 'node:url';
import { defineConfig } from 'vite';

// ES modules for Node's test runner, with the `@` alias resolved the way the app build does: the API layer, and apart
// from it the UI primitives a jsdom test renders, so a test of the API layer loads no React.
export default defineConfig({
  plugins: [react()],
  resolve: { alias: { '@': fileURLToPath(new URL('../src', import.meta.url)) } },
  // A lib bundle serves no public assets, and a copy of them beside it would be picked up as another entry.
  publicDir: false,
  define: { 'import.meta.env.VITE_PHOEBIS_MOCK': '"0"' },
  build: {
    outDir: fileURLToPath(new URL('./dist', import.meta.url)),
    emptyOutDir: true,
    lib: {
      entry: { api: fileURLToPath(new URL('./entry.ts', import.meta.url)), ui: fileURLToPath(new URL('./ui.ts', import.meta.url)) },
      formats: ['es'],
      fileName: (_format, name) => `${name}.js`,
    },
    minify: false,
    target: 'es2022',
  },
});
