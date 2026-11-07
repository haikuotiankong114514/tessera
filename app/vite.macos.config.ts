import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
export default defineConfig({ base: './', plugins: [svelte()], build: {
  target: 'safari17', outDir: '../macos/build/web', emptyOutDir: true,
  rollupOptions: { input: 'macos.html' }, sourcemap: false,
} });
