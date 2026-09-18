import { fileURLToPath, URL } from 'node:url';
import { defineConfig } from 'vite';

// One ES module of the API layer for Node's test runner, with the `@` alias resolved the way the app build does.
export default defineConfig({
  resolve: { alias: { '@': fileURLToPath(new URL('../src', import.meta.url)) } },
  // A lib bundle serves no public assets, and a copy of them beside it would be picked up as another entry.
  publicDir: false,
  define: { 'import.meta.env.VITE_PHOEBIS_MOCK': '"0"' },
  build: {
    outDir: fileURLToPath(new URL('./dist', import.meta.url)),
    emptyOutDir: true,
    lib: { entry: fileURLToPath(new URL('./entry.ts', import.meta.url)), formats: ['es'], fileName: () => 'api.js' },
    minify: false,
    target: 'es2022',
  },
});
