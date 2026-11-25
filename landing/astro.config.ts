import { defineConfig } from 'astro/config';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  site: 'https://po-app.pages.dev',
  vite: {
    plugins: [tailwindcss()],
  },
});
