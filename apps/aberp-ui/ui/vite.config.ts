import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Tauri's dev server expects a fixed port + non-clearing TTY behaviour.
// We do NOT pull in @tauri-apps/api/vite — keeping the config small and
// independent of Tauri-version-specific Vite plugins per CLAUDE.md
// rule 2.
export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  // PR-188 / session 188 — operator-supplied SPA branding lives at
  // `static/aberp-logo.png` and is served at the SPA root
  // (`/aberp-logo.png`). We override Vite's default `public/` to
  // `static/` so the dir name matches the SvelteKit convention the
  // rest of the app docs use.
  publicDir: "static",
  server: {
    port: 5173,
    strictPort: true,
    host: "127.0.0.1",
  },
  build: {
    target: "es2022",
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: true,
  },
});
