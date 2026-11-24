import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Réglages recommandés par Tauri : port fixe, écran conservé pour les messages de Rust, et
// src-tauri hors de la surveillance des fichiers.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    // WebKit sur macOS et Linux, WebView2 (Chromium) sous Windows.
    target: process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome105" : "safari15",
  },
});
