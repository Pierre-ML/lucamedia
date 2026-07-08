// @ts-check
import { defineConfig } from 'astro/config';

import tailwindcss from '@tailwindcss/vite';

// https://astro.build/config
export default defineConfig({
  vite: {
    plugins: [tailwindcss()],
    server: {
      // En dev, le frontend Astro (4321) proxifie l'API vers le backend Express (3000)
      proxy: {
        '/api': 'http://localhost:3000'
      }
    }
  }
});
