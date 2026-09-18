import react from '@vitejs/plugin-react';
import { fileURLToPath, URL } from 'node:url';
import { defineConfig } from 'vite';

// Bundles the app plus a mount helper into one ESM file that the jsdom smoke
// runner can import; CSS is dropped because jsdom does not lay out.
export default defineConfig({
  plugins: [react()],
  resolve: { alias: { '@': fileURLToPath(new URL('../src', import.meta.url)) } },
  // A lib bundle serves no public assets, and a copy of them beside it would be picked up as another entry.
  publicDir: false,
  define: { 'process.env.NODE_ENV': JSON.stringify('development'), 'import.meta.env.VITE_PHOEBIS_MOCK': '"1"' },
  build: {
    outDir: fileURLToPath(new URL('./dist', import.meta.url)),
    emptyOutDir: true,
    lib: { entry: fileURLToPath(new URL('./entry.tsx', import.meta.url)), formats: ['es'], fileName: () => 'entry.js' },
    rollupOptions: { external: [] },
    minify: false,
    cssCodeSplit: false,
    // The api module awaits the mock's dynamic import at the top level, which needs a target that carries it.
    target: 'es2022',
  },
});
