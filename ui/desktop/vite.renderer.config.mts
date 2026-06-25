import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const desktopRoot = fileURLToPath(new URL('.', import.meta.url));
const uiNodeModules = resolve(desktopRoot, '..', 'node_modules');

// https://vitejs.dev/config
export default defineConfig({
  define: {
    'process.env.GOOSE_TUNNEL': JSON.stringify(process.env.GOOSE_TUNNEL !== 'no' && process.env.GOOSE_TUNNEL !== 'none'),
  },

  plugins: [react(), tailwindcss()],

  resolve: {
    alias: {
      react: resolve(uiNodeModules, 'react'),
      'react-dom': resolve(uiNodeModules, 'react-dom'),
    },
    dedupe: ['react', 'react-dom'],
  },

  build: {
    target: 'esnext'
  },
});
