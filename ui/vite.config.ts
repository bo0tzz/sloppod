import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

const target = process.env.SLOPPOD_URL ?? 'http://localhost:8080';

export default defineConfig({
  // The commit or tag this bundle was built from, so the page can tell when the runtime serving it has been redeployed
  // since (#448). The Containerfile's `SLOPPOD_BUILD` build arg carries it, the same value the runtime reports as its
  // build; a build that doesn't pass one (dev, or `npm run build` by hand) leaves the page not knowing its own build.
  define: { __SLOPPOD_BUILD__: JSON.stringify(process.env.SLOPPOD_BUILD ?? '') },
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
