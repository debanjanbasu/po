import { defineConfig } from 'astro/config';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  site: 'https://po-ai.pages.dev',
  vite: {
    plugins: [tailwindcss()],
  },
});
