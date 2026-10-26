import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

const target = process.env.SLOPPOD_URL ?? 'http://localhost:8080';

export default defineConfig({
  plugins: [
    tailwindcss(),
    sveltekit({
      adapter: adapter({ fallback: 'index.html' }),
    }),
  ],
  server: {
    proxy: {
      '/api': { target, changeOrigin: true },
      '/acp': { target, changeOrigin: true, ws: true },
    },
  },
});
