import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri expects a fixed port; fail rather than auto-pick.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: false,
  },
  // Build artifacts go to /dist (Tauri's frontendDist = "../dist")
  build: {
    target: "es2022",
    minify: !process.env.TAURI_DEBUG ? "esbuild" : false,
    sourcemap: !!process.env.TAURI_DEBUG,
  },
});
