import { defineConfig } from 'astro/config';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  site: 'https://oo-app.pages.dev',
  vite: {
    plugins: [tailwindcss()],
  },
});
