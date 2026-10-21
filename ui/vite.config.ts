// SPDX-License-Identifier: MIT OR Apache-2.0

import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vitest/config';

// Tauri expects a fixed dev port and must not be hidden behind Vite's own output.
export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    host: '127.0.0.1',
    // The About screen imports THIRD_PARTY_NOTICES.md from the repository root.
    fs: { allow: ['.', '../THIRD_PARTY_NOTICES.md'] },
  },
  build: {
    target: ['es2022', 'chrome120', 'safari16'],
    sourcemap: false,
  },
  test: {
    include: ['src/**/*.test.ts'],
    environment: 'node',
  },
});
