import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  build: { outDir: 'dist', sourcemap: false },
  // Development only: forward API calls to a local panel-api.
  server: { proxy: { '/api': process.env.SCONTROL_API ?? 'http://127.0.0.1:8080' } },
});
