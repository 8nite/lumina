import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      '/api': 'http://127.0.0.1:7432',
      '/openapi.json': 'http://127.0.0.1:7432',
    },
  },
  build: { outDir: 'dist', sourcemap: false },
});
