import react from '@vitejs/plugin-react';
import { fileURLToPath, URL } from 'node:url';
import { defineConfig } from 'vite';

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) },
  },
  build: { target: 'es2022' },
  // `ws: true` forwards the live feed's WebSocket upgrade, which a bare string target does not.
  server: { proxy: { '/admin': { target: 'http://127.0.0.1:8081', ws: true } } },
});
